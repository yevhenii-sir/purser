using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using NUnit.Framework;
using UnityEngine;

namespace Purser.Tests
{
    public sealed class BillingServiceTests
    {
        private const string User = "c200fdb63a0e9a9b605e424fd437da9249eb198086994a486efa7f115244b8ba";
        private static readonly DateTime Now = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);

        private BillingConfig m_Config;
        private FakeStore m_Store;
        private FakeApi m_Api;
        private List<TimeSpan> m_Delays;
        private List<PurchaseResult> m_Succeeded, m_Pending, m_Failed;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Attribution = null;
            m_Storage = null;
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("vip", BillingProductType.NonConsumable),
                new BillingProductConfig("premium", BillingProductType.Subscription)
                    .WithStoreId(BillingStores.Google, "premium_android").WithStoreId(BillingStores.Apple, "premium_ios"));
            m_Config.WithBuild(BillingStores.Google, "com.test.game").WithBuild(BillingStores.Apple, "com.test.game.ios");
            m_Config.retryDelaysSeconds = new[] { 5, 30 };
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Store = new FakeStore();
            m_Api = new FakeApi();
            m_Delays = new List<TimeSpan>();
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private IAttributionProvider m_Attribution;
        private IBillingStorage m_Storage;

        private BillingService CreateService(string user = User, bool isEditor = false)
        {
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                AttributionProvider = m_Attribution,
                Store = m_Store,
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider(user),
                Storage = m_Storage ?? new MemoryBillingStorage(),
                UtcNow = () => Now,
                Delay = (delay, ct) =>
                {
                    m_Delays.Add(delay);
                    return Task.CompletedTask;
                },
                IsEditor = isEditor,
            });
            m_Succeeded = new List<PurchaseResult>();
            m_Pending = new List<PurchaseResult>();
            m_Failed = new List<PurchaseResult>();
            m_Service.PurchaseSucceeded += m_Succeeded.Add;
            m_Service.PurchasePending += m_Pending.Add;
            m_Service.PurchaseFailed += m_Failed.Add;
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        private PurchaseResult Buy(string productId, StoreOrder order)
        {
            m_Store.OnPurchase = _ => order;
            return Sync.Run(m_Service.PurchaseAsync(productId));
        }

        [Test]
        public void Initialize_LoadsProductsAndBindsAccount()
        {
            CreateService();
            Assert.IsTrue(m_Service.IsInitialized);
            Assert.AreEqual(3, m_Service.Products.Count);
            Assert.AreEqual("premium_android", m_Service.GetProduct("premium").StoreId);
            Assert.AreEqual(User, m_Store.BoundUserId);
        }

        [Test]
        public void Purchase_Success_VerifiesThenConfirms()
        {
            CreateService();
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.IsTrue(result.IsSuccess);
            Assert.AreEqual("gems_100", result.ProductId);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
            Assert.AreEqual(1, m_Succeeded.Count);

            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual(BillingStores.Google, m_Api.VerifyStores.Single());
            Assert.AreEqual("com.test.game", request.BundleId);
            Assert.AreEqual(User, request.UserId);
            Assert.AreEqual("gems_100", request.ProductId);
            Assert.AreEqual("token-1", request.PurchaseToken);
            Assert.AreEqual("GPA.1", request.OrderId);
            Assert.AreEqual(990_000, request.PriceMicros);
            Assert.AreEqual("USD", request.Currency);
        }

        [Test]
        public void Purchase_ServerDelivery_ConfirmsBeforeTheEvent()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success, delivery: VerifyDelivery.Server);
            var confirmedAtEvent = -1;
            m_Service.PurchaseSucceeded += _ => confirmedAtEvent = m_Store.Confirmed.Count;

            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));

            Assert.IsFalse(result.GrantOnClient);
            Assert.AreEqual(1, confirmedAtEvent);
        }

        [Test]
        public void Purchase_ClientDelivery_GrantsInTheEventBeforeConfirming()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success, delivery: VerifyDelivery.Client);
            var confirmedAtEvent = -1;
            m_Service.PurchaseSucceeded += r =>
            {
                Assert.IsTrue(r.GrantOnClient);
                confirmedAtEvent = m_Store.Confirmed.Count;
            };

            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.AreEqual(0, confirmedAtEvent, "the reward is granted before the store purchase is consumed");
            Assert.AreEqual(1, m_Store.Confirmed.Count);
        }

        [Test]
        public void Purchase_ClientDelivery_HandlerFailure_LeavesThePurchaseForTheStoreToDeliverAgain()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success, delivery: VerifyDelivery.Client);
            m_Service.PurchaseSucceeded += _ => throw new InvalidOperationException("save failed");

            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.IsEmpty(m_Store.Confirmed);
        }

        [Test]
        public void Purchase_UsesPlatformSpecificStoreId()
        {
            CreateService();
            m_Api.EntitlementsResult = FakeApi.Entitlements(new Dictionary<string, long>());
            Buy("premium", FakeStore.GoogleOrder("premium_android"));
            Assert.AreEqual("premium_android", m_Store.PurchaseCalls.Single());
            Assert.AreEqual("premium_android", m_Api.VerifyRequests.Single().ProductId);
        }

        [Test]
        public void Purchase_Pending202_ConfirmsAsPendingDelivery()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.Verify(202, VerifyStatus.Pending);
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(PurchaseStatus.PendingDelivery, result.Status);
            Assert.IsTrue(result.IsSuccess);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
        }

        [Test]
        public void Purchase_PaymentPending_IsNotConfirmed()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.PendingOrCanceled);
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
            Assert.IsEmpty(m_Store.Confirmed);
            Assert.AreEqual(1, m_Pending.Count);
        }

        [Test]
        public void Purchase_Invalid_IsConfirmedAndRejected()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.Verify(400, VerifyStatus.Invalid, "Invalid Value");
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(PurchaseStatus.Rejected, result.Status);
            Assert.AreEqual(1, m_Store.Confirmed.Count, "rejected orders are finished so the store stops re-delivering them");
            Assert.IsFalse(m_Store.FinishedGranted.Single(), "finished as not granted (Amazon: UNAVAILABLE)");
            Assert.AreEqual(1, m_Failed.Count);
        }

        [Test]
        public void Purchase_Granted_IsFinishedAsGranted()
        {
            CreateService();
            Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.IsTrue(m_Store.FinishedGranted.Single());
        }

        [Test]
        public void OrderWithoutProduct_TakesTheProductFromTheServer()
        {
            CreateService();
            m_Api.DefaultVerify = new ApiResult<VerifyResponse>(200, new VerifyResponse
            {
                Status = VerifyStatus.Success, TransactionId = "GPA.9", ProductId = "premium_android",
            }, null);

            m_Store.Deliver(new StoreOrder(BillingPlatform.Google, "", "GPA.9", "tok-9"));

            Assert.AreEqual("premium", m_Succeeded.Single().ProductId, "not the first configured product");
        }

        [Test]
        public void EmptyStoreIdsNeverMatch()
        {
            // As the inspector saves it: an override row with an empty id.
            var inspectorMade = new BillingProductConfig { id = "gems_100", type = BillingProductType.Consumable };
            inspectorMade.storeIds.Add(new StoreProductId(BillingStores.Apple, ""));
            Assert.IsFalse(inspectorMade.Matches(""));
            Assert.IsFalse(inspectorMade.Matches(null));
            Assert.AreEqual("gems_100", inspectorMade.StoreId(BillingStores.Apple));
            Assert.IsNull(m_Config.FindByStoreId(""));
        }

        [Test]
        public void StoreFailureWithoutProduct_FailsEveryOpenPurchase()
        {
            CreateService();
            var gems = m_Service.PurchaseAsync("gems_100");
            var vip = m_Service.PurchaseAsync("vip");
            Assert.IsFalse(gems.IsCompleted);

            m_Store.Fail(null, cancelled: false);

            Assert.AreEqual(PurchaseStatus.Failed, Sync.Run(gems).Status);
            Assert.AreEqual(PurchaseStatus.Failed, Sync.Run(vip).Status);
        }

        [Test]
        public void GameHandlerExceptions_DoNotReachTheStoreCallbacks()
        {
            CreateService();
            m_Service.PurchasePending += _ => throw new InvalidOperationException("ui bug");
            m_Service.PurchaseFailed += _ => throw new InvalidOperationException("ui bug");
            Assert.DoesNotThrow(() => m_Store.Defer(FakeStore.GoogleOrder("vip")));
            Assert.DoesNotThrow(() => m_Store.Fail("gems_100", cancelled: false));
        }

        [Test]
        public void Purchase_ServerDelivery_HandlerFailure_IsNotReportedAsPendingAfterTheSuccess()
        {
            CreateService();
            m_Service.PurchaseSucceeded += _ => throw new InvalidOperationException("ui bug");
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
            Assert.IsEmpty(m_Pending);
        }

        [Test]
        public void Purchase_ServerError_RetriesWithoutConfirmingUntilAccepted()
        {
            CreateService();
            m_Api.VerifyResults.Enqueue(FakeApi.TransportError());
            m_Api.VerifyResults.Enqueue(FakeApi.Verify(400, VerifyStatus.Error, "unknown product"));
            m_Api.VerifyResults.Enqueue(FakeApi.Verify(200, VerifyStatus.Success));

            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));

            Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status, "caller is answered after the first failure");
            Assert.AreEqual(3, m_Api.VerifyRequests.Count);
            CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(5), TimeSpan.FromSeconds(30) }, m_Delays);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
            Assert.AreEqual(1, m_Succeeded.Count, "success is still reported through the event");
        }

        [Test]
        public void Purchase_ServerUnreachable_GivesUpWithoutConfirming()
        {
            CreateService();
            m_Api.DefaultVerify = FakeApi.TransportError();
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
            Assert.AreEqual(m_Config.retryDelaysSeconds.Length + 1, m_Api.VerifyRequests.Count);
            Assert.IsEmpty(m_Store.Confirmed, "the store must deliver the order again on next launch");
        }

        [Test]
        public void SameOrderDeliveredTwiceWhileVerifying_IsVerifiedOnce()
        {
            CreateService();
            var hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
            m_Api.Hold = hold;
            var order = FakeStore.GoogleOrder("gems_100");
            m_Store.Deliver(order);
            m_Store.Deliver(order);
            Assert.AreEqual(1, m_Api.VerifyRequests.Count);

            hold.SetResult(FakeApi.Verify(200, VerifyStatus.Success));
            Assert.AreEqual(1, m_Store.Confirmed.Count);
            Assert.AreEqual(1, m_Succeeded.Count);
        }

        [Test]
        public void OrderBeforeUserId_WaitsAndIsVerifiedForThatUser()
        {
            CreateService(user: null);
            m_Store.Deliver(FakeStore.GoogleOrder("vip"));
            Assert.IsEmpty(m_Api.VerifyRequests);

            m_Service.SetUserId("player-42");
            Assert.AreEqual("player-42", m_Api.VerifyRequests.Single().UserId);
            Assert.AreEqual("player-42", m_Store.BoundUserId);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
        }

        [Test]
        public void Purchase_WithoutUser_Fails()
        {
            CreateService(user: null);
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            Assert.IsEmpty(m_Store.PurchaseCalls);
        }

        [Test]
        public void Purchase_UserCancelled()
        {
            CreateService();
            m_Store.OnPurchase = id =>
            {
                m_Store.Fail(id, cancelled: true);
                return null;
            };
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Cancelled, result.Status);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void Purchase_Deferred_IsPendingPayment()
        {
            CreateService();
            m_Store.OnPurchase = id =>
            {
                m_Store.Defer(FakeStore.GoogleOrder(id));
                return null;
            };
            var result = Sync.Run(m_Service.PurchaseAsync("vip"));
            Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
        }

        [Test]
        public void Purchase_UnknownProductOrNotInitialized()
        {
            m_Service = new BillingService(m_Config, new BillingDependencies { Store = m_Store, Api = m_Api, IsEditor = false });
            Assert.AreEqual(PurchaseStatus.NotInitialized, Sync.Run(m_Service.PurchaseAsync("gems_100")).Status);
            m_Service.Dispose();

            CreateService();
            Assert.AreEqual(PurchaseStatus.Failed, Sync.Run(m_Service.PurchaseAsync("nope")).Status);
        }

        [Test]
        public void Apple_SendsTransactionIdToAppleRoute()
        {
            m_Store.Platform = BillingPlatform.Apple;
            CreateService();
            Buy("premium", FakeStore.AppleOrder("premium_ios"));
            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual(BillingStores.Apple, m_Api.VerifyStores.Single());
            Assert.AreEqual("com.test.game.ios", request.BundleId);
            Assert.AreEqual("2000000001", request.OrderId);
            Assert.IsNull(request.PurchaseToken);
        }

        [Test]
        public void Editor_SkipServer_ConfirmsLocallyAndGrantsEntitlement()
        {
            CreateService(isEditor: true);
            var result = Buy("vip", FakeStore.GoogleOrder("vip"));
            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.IsEmpty(m_Api.VerifyRequests);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
            Assert.IsTrue(m_Service.IsActive("vip"));
        }

        [Test]
        public void Entitlements_MapConfiguredIdsAcrossPlatforms()
        {
            CreateService();
            var expires = new DateTimeOffset(Now.AddDays(10)).ToUnixTimeMilliseconds();
            m_Api.EntitlementsResult = FakeApi.Entitlements(new Dictionary<string, long> { ["premium_ios"] = expires }, "vip");
            m_Api.EntitlementsResult.Value.ServerTimeMs = new DateTimeOffset(Now).ToUnixTimeMilliseconds();

            var ent = Sync.Run(m_Service.RefreshEntitlementsAsync());
            Assert.IsFalse(ent.IsFromCache);
            Assert.IsTrue(m_Service.IsActive("premium"), "bought on iOS, checked on Android");
            Assert.IsTrue(m_Service.IsActive("vip"));
            Assert.IsFalse(m_Service.IsActive("gems_100"));
            Assert.AreEqual(Now.AddDays(10), m_Service.GetExpiration("premium"));
        }

        [Test]
        public void Entitlements_ServerDown_ReturnsCachedCopy()
        {
            CreateService();
            m_Api.EntitlementsResult = FakeApi.Entitlements(new Dictionary<string, long>(), "vip");
            Sync.Run(m_Service.RefreshEntitlementsAsync());

            m_Api.EntitlementsResult = new ApiResult<EntitlementsDto>(0, null, "offline");
            var ent = Sync.Run(m_Service.RefreshEntitlementsAsync());
            Assert.IsTrue(ent.IsFromCache);
            Assert.IsTrue(ent.Owns("vip"));
        }

        [Test]
        public void Restore_SendsOwnedPurchasesAndUpdatesEntitlements()
        {
            CreateService();
            m_Store.Owned.Add(FakeStore.GoogleOrder("vip", "tok-a", "GPA.A"));
            m_Store.Owned.Add(FakeStore.GoogleOrder("premium_android", "tok-b", "GPA.B"));
            m_Store.Owned.Add(FakeStore.GoogleOrder("premium_android", "tok-b", "GPA.B"));
            m_Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse
            {
                Results = new List<RestoreItemResult>(),
                Entitlements = FakeApi.Entitlements(new Dictionary<string, long>(), "vip").Value,
            }, null);

            var ent = Sync.Run(m_Service.RestoreAsync());

            var request = m_Api.RestoreRequests.Single();
            Assert.AreEqual("com.test.game", request.AppId);
            Assert.AreEqual(User, request.UserId);
            CollectionAssert.AreEquivalent(new[] { "tok-a", "tok-b" }, request.Purchases.Select(p => p.PurchaseToken));
            Assert.IsTrue(ent.Owns("vip"));
            Assert.IsFalse(m_Store.RestoreCalled, "RestoreTransactions is only needed on Apple");
        }

        [Test]
        public void Attribution_IsSentWithVerifyAndRestore()
        {
            m_Attribution = new FuncAttributionProvider(() => new Dictionary<string, string>
            {
                [AttributionKeys.AppsFlyerId] = "1700000000000-1234567890123456789",
                [AttributionKeys.AdvertisingId] = "",
            });
            CreateService();
            Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            var sent = m_Api.VerifyRequests.Single().Attribution;
            Assert.AreEqual("1700000000000-1234567890123456789", sent[AttributionKeys.AppsFlyerId]);
            Assert.IsFalse(sent.ContainsKey(AttributionKeys.AdvertisingId), "empty values are dropped");

            m_Store.Owned.Add(FakeStore.GoogleOrder("vip", "tok-a", "GPA.A"));
            Sync.Run(m_Service.RestoreAsync());
            Assert.AreEqual("1700000000000-1234567890123456789",
                m_Api.RestoreRequests.Single().Attribution[AttributionKeys.AppsFlyerId]);
        }

        [Test]
        public void RefundDataConsent_IsSentWithVerifyAndKeptOnTheDevice()
        {
            m_Storage = new MemoryBillingStorage();
            CreateService();
            Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.IsNull(m_Api.VerifyRequests.Single().Attribution, "nothing is sent before the player answers");

            m_Service.SetRefundDataConsent(true);
            Buy("gems_100", FakeStore.GoogleOrder("gems_100", "tok-2", "GPA.2"));
            Assert.AreEqual("true", m_Api.VerifyRequests.Last().Attribution[AttributionKeys.RefundDataConsent]);

            // A new session on the same device sends the stored answer; a withdrawal is sent as "false".
            m_Service.Dispose();
            CreateService();
            m_Service.SetRefundDataConsent(false);
            Buy("gems_100", FakeStore.GoogleOrder("gems_100", "tok-3", "GPA.3"));
            Assert.AreEqual("false", m_Api.VerifyRequests.Last().Attribution[AttributionKeys.RefundDataConsent]);
        }

        [Test]
        public void Attribution_ProviderFailureDoesNotBlockPurchase()
        {
            m_Attribution = new FuncAttributionProvider(() => throw new InvalidOperationException("sdk not ready"));
            CreateService();
            var result = Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.IsNull(m_Api.VerifyRequests.Single().Attribution);
        }

        [Test]
        public void FallbackPrice_IsShownWhenTheStoreHasNone_AndNotSentToTheServer()
        {
            m_Store.NoPrices = true;
            m_Config.products[0].WithFallbackPrice(1.99, "eur");
            CreateService();

            var gems = m_Service.GetProduct("gems_100");
            Assert.IsTrue(gems.IsFallbackPrice);
            Assert.AreEqual((1.99m, "EUR", "1.99 EUR"), (gems.Price, gems.Currency, gems.LocalizedPrice));
            Assert.IsFalse(m_Service.GetProduct("vip").IsFallbackPrice, "no fallback configured");

            Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            Assert.AreEqual(0, m_Api.VerifyRequests.Single().PriceMicros, "the game's guess is not reported as revenue");
            Assert.IsNull(m_Api.VerifyRequests.Single().Currency);
        }

        [Test]
        public void FallbackPeriod_IsUsedWhenTheStoreHasNone()
        {
            m_Config.FindById("premium").WithFallbackPeriod("p1m");
            CreateService();

            var premium = m_Service.GetProduct("premium");
            Assert.AreEqual(("P1M", 30, true), (premium.SubscriptionPeriod, premium.SubscriptionPeriodDays, premium.IsFallbackPeriod));
            Assert.IsFalse(premium.IsFallbackPrice, "the store's price stays");
            Assert.AreEqual(("", false), (m_Service.GetProduct("gems_100").SubscriptionPeriod, m_Service.GetProduct("gems_100").IsFallbackPeriod));
        }

        [Test]
        public void StorePeriod_WinsOverTheFallback()
        {
            m_Store.Period = "P1Y";
            m_Config.FindById("premium").WithFallbackPeriod("P1M");
            CreateService();
            var premium = m_Service.GetProduct("premium");
            Assert.AreEqual(("P1Y", 365, false), (premium.SubscriptionPeriod, premium.SubscriptionPeriodDays, premium.IsFallbackPeriod));
        }

        [Test]
        public void StorePrice_WinsOverTheFallback()
        {
            m_Config.products[0].WithFallbackPrice(1.99, "EUR");
            CreateService();
            var gems = m_Service.GetProduct("gems_100");
            Assert.IsFalse(gems.IsFallbackPrice);
            Assert.AreEqual((0.99m, "USD"), (gems.Price, gems.Currency));
        }

        [Test]
        public void Store_NamesTheRouteAndTheBuild()
        {
            m_Store.Store = BillingStores.Amazon;
            m_Config.WithBuild(BillingStores.Amazon, "com.test.game.amazon");
            CreateService();
            Assert.AreEqual(BillingStores.Amazon, m_Service.Store);
            Buy("gems_100", new StoreOrder(BillingPlatform.Amazon, "gems_100", "receipt-1", null));
            Assert.AreEqual(BillingStores.Amazon, m_Api.VerifyStores.Single());
            Assert.AreEqual("com.test.game.amazon", m_Api.VerifyRequests.Single().BundleId);
        }

        [Test]
        public void Requests_CarryTheDevice()
        {
            CreateService();
            Buy("gems_100", FakeStore.GoogleOrder("gems_100"));
            var client = m_Api.VerifyRequests.Single().Client;
            Assert.AreEqual((SystemInfo.deviceModel, SystemInfo.operatingSystem), (client.Device, client.Os));
            Assert.AreEqual((Application.version, PurserInfo.Version), (client.AppVersion, client.SdkVersion));
        }

        [Test]
        public void CustomAttributes_AreSentForTheCurrentPlayer()
        {
            CreateService();
            var ok = Sync.Run(m_Service.SetCustomAttributesAsync(new Dictionary<string, object> { ["email"] = "player@example.com", ["level"] = 7, ["old"] = null }));
            Assert.IsTrue(ok);
            var request = m_Api.AttributeRequests.Single();
            Assert.AreEqual(("com.test.game", User), (request.AppId, request.UserId));
            Assert.AreEqual(7, request.Attributes["level"]);
            var body = Newtonsoft.Json.Linq.JObject.Parse(Newtonsoft.Json.JsonConvert.SerializeObject(request));
            Assert.AreEqual(Newtonsoft.Json.Linq.JTokenType.Null, body["attributes"]["old"].Type, "null removes the key on the server");

            m_Api.AttributesResult = new ApiResult<AttributesResponse>(400, new AttributesResponse { Status = "error", Message = "bad key" }, "HTTP 400");
            Assert.IsFalse(Sync.Run(m_Service.SetCustomAttributesAsync(new Dictionary<string, object> { ["bad key"] = 1 })));
        }

        [Test]
        public void CustomAttributes_NeedAUser()
        {
            CreateService(user: null);
            Assert.IsFalse(Sync.Run(m_Service.SetCustomAttributesAsync(new Dictionary<string, object> { ["level"] = 1 })));
            Assert.IsEmpty(m_Api.AttributeRequests);
        }

        [Test]
        public void SdkVersion_MatchesThePackage()
        {
            var info = UnityEditor.PackageManager.PackageInfo.FindForAssembly(typeof(PurserInfo).Assembly);
            Assert.IsNotNull(info);
            Assert.AreEqual(info.version, PurserInfo.Version, "bump PurserInfo.Version with package.json");
        }

        [Test]
        public void Dispose_DisposesStore()
        {
            CreateService();
            m_Service.Dispose();
            Assert.IsTrue(m_Store.Disposed);
        }
    }
}
