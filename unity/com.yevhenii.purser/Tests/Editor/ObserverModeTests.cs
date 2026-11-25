using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Observer mode (<see cref="IBillingService.ReportPurchaseAsync"/>) and fail-open grants.</summary>
    public sealed class ObserverModeTests
    {
        private const string GoogleReceipt =
            "{\"Store\":\"GooglePlay\",\"TransactionID\":\"GPA.7\",\"Payload\":\"{\\\"json\\\":\\\"{\\\\\\\"orderId\\\\\\\":\\\\\\\"GPA.7\\\\\\\",\\\\\\\"purchaseToken\\\\\\\":\\\\\\\"tok-7\\\\\\\"}\\\",\\\"signature\\\":\\\"sig\\\"}\"}";

        private static ExternalPurchase Google(string storeId = "gems_100", string token = "tok-7") =>
            new ExternalPurchase { Store = BillingStores.Google, StoreProductId = storeId, PurchaseToken = token, TransactionId = "GPA.7", Price = 0.99m, Currency = "USD" };

        [Test]
        public void FromUnityReceipt_ReadsGoogleAppleAndAmazonReceipts()
        {
            var google = ExternalPurchase.FromUnityReceipt("gems_100", GoogleReceipt, 0.99m, "USD");
            Assert.AreEqual((BillingStores.Google, "tok-7", "GPA.7"), (google.Store, google.PurchaseToken, google.TransactionId));
            Assert.AreEqual(990_000, google.PriceMicros);

            var apple = ExternalPurchase.FromUnityReceipt("gems", "{\"Store\":\"AppleAppStore\",\"TransactionID\":\"2000000123\",\"Payload\":\"MIIT\"}");
            Assert.AreEqual((BillingStores.Apple, "2000000123", (string)null), (apple.Store, apple.TransactionId, apple.PurchaseToken));

            var amazon = ExternalPurchase.FromUnityReceipt("gems", "{\"Store\":\"AmazonApps\",\"TransactionID\":\"receipt-1\",\"Payload\":\"{}\"}");
            Assert.AreEqual(BillingStores.Amazon, amazon.Store);
            Assert.IsNotNull(amazon.Problem(), "Amazon needs the Amazon user id too");

            Assert.IsNull(ExternalPurchase.FromUnityReceipt("gems", "not json"));
            Assert.IsNull(ExternalPurchase.FromUnityReceipt("gems", "{\"Store\":\"WinRT\",\"TransactionID\":\"1\"}"));
        }

        [Test]
        public void RecordOnly_VerifiesWithClientFinishedAndRaisesVerified()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var verified = new List<PurchaseResult>();
                h.Service.PurchaseVerified += verified.Add;

                var result = Sync.Run(h.Service.ReportPurchaseAsync(Google()));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                Assert.AreEqual("gems_100", result.ProductId);
                var request = h.Api.VerifyRequests.Single();
                Assert.IsTrue(request.ClientFinished);
                Assert.AreEqual(("tok-7", "GPA.7", 990_000L), (request.PurchaseToken, request.OrderId, request.PriceMicros));
                Assert.AreEqual(ServiceHarness.AppId, request.AppId);
                Assert.AreEqual(BillingStores.Google, h.Api.VerifyStores.Single());
                Assert.AreEqual(1, verified.Count);
                Assert.IsEmpty(h.Store.Confirmed, "the game finishes its own purchases");
                Assert.IsEmpty(h.Succeeded, "PurchaseSucceeded is for purchases Purser made");
                Assert.IsTrue(string.IsNullOrEmpty(h.Storage.Load(ObservedPurchases.StorageKey)), "nothing left to verify");
            }
        }

        [Test]
        public void RecordOnly_KeepsThePurchaseAcrossARestartWhileTheServerIsDown()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                var result = Sync.Run(h.Service.ReportPurchaseAsync(Google()));
                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
                Assert.AreEqual(3, h.Api.VerifyRequests.Count, "the first try and one per retry delay");
                StringAssert.Contains("tok-7", h.Storage.Load(ObservedPurchases.StorageKey));

                // The app is killed and started again; the server is back.
                h.Service.Dispose();
                h.Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success);
                var verified = new List<PurchaseResult>();
                h.Start(initialize: false);
                h.Service.PurchaseVerified += verified.Add;
                Sync.Run(h.Service.InitializeAsync());

                Assert.AreEqual(1, verified.Count);
                Assert.IsTrue(h.Api.VerifyRequests.Last().ClientFinished);
                Assert.IsTrue(string.IsNullOrEmpty(h.Storage.Load(ObservedPurchases.StorageKey)));
            }
        }

        [Test]
        public void RecordOnly_RejectedPurchaseRaisesRejectedAndIsForgotten()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var rejected = new List<PurchaseResult>();
                h.Service.PurchaseRejected += rejected.Add;
                h.Api.DefaultVerify = FakeApi.Verify(400, VerifyStatus.Invalid, "not a real purchase");

                var result = Sync.Run(h.Service.ReportPurchaseAsync(Google()));

                Assert.AreEqual(PurchaseStatus.Rejected, result.Status);
                Assert.AreEqual(1, rejected.Count);
                Assert.IsFalse(rejected[0].Provisional);
                Assert.IsTrue(string.IsNullOrEmpty(h.Storage.Load(ObservedPurchases.StorageKey)));
            }
        }

        [Test]
        public void ReportPurchase_WithoutAPlayerIsKeptUntilOne()
        {
            using (var h = new ServiceHarness())
            {
                h.Users = new StaticUserIdProvider(null);
                h.Start();
                var result = Sync.Run(h.Service.ReportPurchaseAsync(Google()));
                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
                Assert.IsEmpty(h.Api.VerifyRequests);

                h.Service.SetUserId("player-9");
                Assert.AreEqual("player-9", h.Api.VerifyRequests.Single().UserId);
            }
        }

        [Test]
        public void ReportPurchase_RefusesWhatTheServerCannotVerify()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var result = Sync.Run(h.Service.ReportPurchaseAsync(new ExternalPurchase { Store = BillingStores.Google, StoreProductId = "gems_100" }));
                Assert.AreEqual(PurchaseStatus.Failed, result.Status);
                StringAssert.Contains("purchase token", result.Message);
                Assert.IsEmpty(h.Api.VerifyRequests);
            }
        }

        [Test]
        public void VerifyFirst_GivesAProvisionalSuccessWhenTheServerIsSlow()
        {
            using (var h = new ServiceHarness())
            {
                h.Config.failOpenAfterSeconds = 8;
                h.Start();
                h.Api.Hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();

                var result = Sync.Run(h.Service.ReportPurchaseAsync(Google("no_ads", "tok-8"), ReportMode.VerifyFirst));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                Assert.IsTrue(result.Provisional);
                Assert.IsTrue(result.GrantOnClient);
                Assert.IsTrue(h.Service.HasAccess("no_ads"), "granted locally until the server answers");
                StringAssert.Contains("tok-8", h.Storage.Load(ObservedPurchases.StorageKey));
            }
        }

        [Test]
        public void VerifyFirst_StrictProductsWaitForTheServer()
        {
            using (var h = new ServiceHarness())
            {
                h.Config.failOpenAfterSeconds = 8;
                h.Config.products.First(p => p.id == "no_ads").strictVerification = true;
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();

                var result = Sync.Run(h.Service.ReportPurchaseAsync(Google("no_ads", "tok-8"), ReportMode.VerifyFirst));

                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
                Assert.IsFalse(result.Provisional);
                Assert.IsFalse(h.Service.HasAccess("no_ads"));
            }
        }

        [Test]
        public void FailOpen_GrantsAStorePurchaseAndRejectsItLaterWhenTheServerRefuses()
        {
            using (var h = new ServiceHarness())
            {
                h.Config.failOpenAfterSeconds = 8;
                h.Storage.Save(OrderProcessor.DeliveryStorageKey, VerifyDelivery.Client);
                h.Start();
                var rejected = new List<PurchaseResult>();
                h.Service.PurchaseRejected += rejected.Add;
                h.Api.Hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
                h.Api.DefaultVerify = FakeApi.Verify(400, VerifyStatus.Invalid, "refunded");
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id, "tok-9", "GPA.9");

                var result = Sync.Run(h.Service.PurchaseAsync("no_ads"));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                Assert.IsTrue(result.Provisional);
                Assert.AreEqual(1, h.Store.Confirmed.Count, "finished like a success");
                // The kept purchase was verified right away (client_finished) and refused.
                Assert.IsTrue(h.Api.VerifyRequests.Last().ClientFinished);
                Assert.AreEqual(1, rejected.Count);
                Assert.IsTrue(rejected[0].Provisional);
            }
        }

        [Test]
        public void FailOpen_WaitsUntilTheServerSaidTheClientGrants()
        {
            using (var h = new ServiceHarness())
            {
                // With a server delivery, a purchase the client finished could not be delivered by the server.
                h.Config.failOpenAfterSeconds = 8;
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id, "tok-9", "GPA.9");

                var result = Sync.Run(h.Service.PurchaseAsync("no_ads"));

                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
                Assert.IsEmpty(h.Store.Confirmed);
            }
        }

        [Test]
        public void KeptPurchases_AreVerifiedForTheirBuyer()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                Sync.Run(h.Service.ReportPurchaseAsync(Google("no_ads", "tok-5")));
                h.Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success);

                h.Service.SetUserId("player-2");

                Assert.AreEqual("player-1", h.Api.VerifyRequests.Last().UserId, "the buyer, not the player signed in now");
            }
        }

        [Test]
        public void SetUserId_RefusesIdsTheServerDoesNotAccept()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                Assert.Throws<ArgumentException>(() => h.Service.SetUserId("player@example.com"));
                Assert.IsTrue(AccountIds.IsValidUserId("player-1.x:y_z"));
                Assert.IsFalse(AccountIds.IsValidUserId(new string('a', 129)));
            }
        }

        [Test]
        public void FailOpen_IsOffByDefault()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id, "tok-9", "GPA.9");

                var result = Sync.Run(h.Service.PurchaseAsync("no_ads"));

                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
                Assert.IsEmpty(h.Store.Confirmed);
            }
        }

        [Test]
        public void ReportRestored_SendsOneRestorePerStoreWithClientFinished()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse(), null);
                h.Answer(null, new Dictionary<string, double> { ["no_ads"] = 0 }, "no_ads");

                var info = Sync.Run(h.Service.ReportRestoredAsync(new[]
                {
                    Google("no_ads", "tok-1"),
                    new ExternalPurchase { Store = BillingStores.Apple, StoreProductId = "premium_ios", TransactionId = "2000000001" },
                }));

                Assert.AreEqual(2, h.Api.RestoreRequests.Count);
                Assert.IsTrue(h.Api.RestoreRequests.All(r => r.ClientFinished));
                Assert.AreEqual("com.test.game.ios", h.Api.RestoreRequests.Single(r => r.Purchases[0].OrderId == "2000000001").AppId);
                Assert.IsNotNull(info);
                Assert.IsTrue(h.Service.HasAccess("no_ads"));
            }
        }

        [Test]
        public void ExternalStoreAdapter_RunsTheServiceWithoutUnityIap()
        {
            var config = BillingConfig.Create("https://billing.example.com/route", new BillingProductConfig("gems_100", BillingProductType.Consumable));
            var store = new ExternalStoreAdapter(BillingStores.Google, BillingPlatform.Google);
            store.SetPrice("gems_100", 1.99m, "EUR", "1,99 €");
            try
            {
                using (var service = new BillingService(config, new BillingDependencies
                {
                    Store = store,
                    Api = new FakeApi(),
                    UserIdProvider = new StaticUserIdProvider("player-1"),
                    Storage = new MemoryBillingStorage(),
                    CreateLifecycleHook = false,
                    LogSink = (l, m) => { },
                }))
                {
                    Sync.Run(service.InitializeAsync());
                    Assert.AreEqual("1,99 €", service.GetProduct("gems_100").LocalizedPrice);
                    var buy = Sync.Run(service.PurchaseAsync("gems_100"));
                    Assert.AreEqual(PurchaseStatus.Failed, buy.Status);
                    StringAssert.Contains("observer mode", buy.Message);
                }
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(config);
            }
        }
    }
}
