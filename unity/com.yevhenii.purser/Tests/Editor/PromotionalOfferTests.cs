using System.Linq;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    public sealed class PromotionalOfferTests
    {
        private const string User = "player-42";

        private BillingConfig m_Config;
        private FakeStore m_Store;
        private FakeApi m_Api;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("premium", BillingProductType.Subscription, "premium_android", "premium_ios"));
            m_Config.androidAppId = "com.test.game";
            m_Config.iosAppId = "com.test.game.ios";
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Store = new FakeStore { Platform = BillingPlatform.Apple };
            m_Api = new FakeApi();
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService CreateService(string user = User)
        {
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = m_Store,
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider(user),
                Storage = new MemoryBillingStorage(),
                Delay = (delay, ct) => Task.CompletedTask,
                IsEditor = false,
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void Purchase_SignsTheOfferForThePlayerAndBuysWithIt()
        {
            CreateService();
            m_Store.OnPurchase = id => FakeStore.AppleOrder(id);

            var result = Sync.Run(m_Service.PurchaseWithPromotionalOfferAsync("premium", "winback_50"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            var request = m_Api.OfferRequests.Single();
            Assert.AreEqual("com.test.game.ios", request.BundleId);
            Assert.AreEqual("premium_ios", request.ProductId);
            Assert.AreEqual("winback_50", request.OfferId);
            Assert.AreEqual(User, request.UserId);

            var (storeId, offer) = m_Store.OfferPurchases.Single();
            Assert.AreEqual("premium_ios", storeId);
            Assert.AreEqual("KEY123ABCD", offer.KeyId);
            Assert.AreEqual(AccountIds.AppleAccountToken(User).ToString(), offer.AppAccountToken);
            Assert.IsEmpty(m_Store.PurchaseCalls, "no purchase without the offer");
            Assert.AreEqual("premium_ios", m_Api.VerifyRequests.Single().ProductId);
            Assert.AreEqual(1, m_Store.Confirmed.Count);
        }

        [Test]
        public void Purchase_OnGooglePlay_Throws()
        {
            m_Store.Platform = BillingPlatform.Google;
            CreateService();
            var e = Assert.Throws<BillingException>(() => Sync.Run(m_Service.PurchaseWithPromotionalOfferAsync("premium", "winback_50")));
            StringAssert.Contains("offer token", e.Message);
            Assert.IsEmpty(m_Api.OfferRequests);
        }

        [Test]
        public void Purchase_ServerRefusal_FailsWithoutBuyingAndReleasesTheProduct()
        {
            CreateService();
            m_Api.OfferResult = _ => new ApiResult<ApplePromotionalOfferResponse>(500,
                new ApplePromotionalOfferResponse { Status = VerifyStatus.Error, Message = "[ERR_OFFER_KEY] not configured" }, "HTTP 500");

            var result = Sync.Run(m_Service.PurchaseWithPromotionalOfferAsync("premium", "winback_50"));

            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            StringAssert.Contains("ERR_OFFER_KEY", result.Message);
            Assert.IsEmpty(m_Store.OfferPurchases);
            Assert.IsEmpty(m_Store.PurchaseCalls);

            m_Api.OfferResult = FakeApi.SignedOffer;
            m_Store.OnPurchase = id => FakeStore.AppleOrder(id);
            Assert.AreEqual(PurchaseStatus.Success, Sync.Run(m_Service.PurchaseWithPromotionalOfferAsync("premium", "winback_50")).Status);
        }

        [Test]
        public void Purchase_OfferForAnotherAccountToken_IsNotUsed()
        {
            CreateService();
            m_Api.OfferResult = r =>
            {
                var signed = FakeApi.SignedOffer(r);
                signed.Value.AppAccountToken = AccountIds.AppleAccountToken("someone-else").ToString();
                return signed;
            };

            var result = Sync.Run(m_Service.PurchaseWithPromotionalOfferAsync("premium", "winback_50"));

            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            Assert.IsEmpty(m_Store.OfferPurchases);
        }

        [Test]
        public void Purchase_NonSubscription_FailsWithoutAskingTheServer()
        {
            CreateService();
            var result = Sync.Run(m_Service.PurchaseWithPromotionalOfferAsync("gems_100", "winback_50"));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            Assert.IsEmpty(m_Api.OfferRequests);
        }

        [Test]
        public void GetOffer_ReturnsTheSignedOffer()
        {
            CreateService();
            var offer = Sync.Run(m_Service.GetApplePromotionalOfferAsync("premium", "winback_50"));
            Assert.AreEqual("premium_ios", offer.ProductId);
            Assert.AreEqual("eyJhbGciOiJFUzI1NiJ9.e30.sig", offer.SignedJws);
            Assert.AreEqual(new System.DateTime(2027, 1, 16, 8, 0, 0, System.DateTimeKind.Utc), offer.ExpiresAt);
        }

        [Test]
        public void UnityIapOptions_HoldTheOfferAtTheTopLevel()
        {
            var offer = FakeApi.SignedOffer(new ApplePromotionalOfferRequest { ProductId = "premium_ios", OfferId = "winback_50", UserId = User })
                .Value.ToOffer();
            var json = JObject.Parse(UnityIapApplePromotionalOffers.BuildOptionsJson(offer));

            Assert.AreEqual("winback_50", (string)json["offerID"]);
            Assert.AreEqual("KEY123ABCD", (string)json["keyID"]);
            Assert.AreEqual("a1b2c3d4-0000-4000-8000-00000000000f", (string)json["nonce"]);
            Assert.AreEqual("MEUCIQD0ZXN0", (string)json["signature"]);
            Assert.AreEqual(JTokenType.Integer, json["timestamp"].Type, "StoreKit bridge reads an Int");
            Assert.AreEqual(1_800_000_000_000, (long)json["timestamp"]);
            Assert.AreEqual(AccountIds.AppleAccountToken(User).ToString(), (string)json["appAccountToken"]);
            Assert.AreEqual(false, (bool)json["simulatesAskToBuyInSandbox"]);
        }

        [Test]
        public void UnityIap_HasTheInternalsTheOfferBridgeUses()
        {
            Assert.IsNull(UnityIapApplePromotionalOffers.MissingInternals());
        }

        [Test]
        public void UnityIapOptions_RejectAMalformedOffer()
        {
            var bad = new ApplePromotionalOffer("premium_ios", "winback_50", "KEY123ABCD", "not-a-uuid", 1, "MEUCIQD0ZXN0",
                AccountIds.AppleAccountToken(User).ToString());
            Assert.IsFalse(bad.IsWellFormed(out _));
            Assert.Throws<BillingException>(() => UnityIapApplePromotionalOffers.BuildOptionsJson(bad));
        }

        [Test]
        public void ApiClient_PostsToTheOfferRoute()
        {
            var transport = new FakeTransport
            {
                Next = new HttpResponse(200, "{\"status\":\"success\",\"product_id\":\"premium_ios\",\"offer_id\":\"winback_50\"," +
                                             "\"key_id\":\"KEY123ABCD\",\"nonce\":\"a1b2c3d4-0000-4000-8000-00000000000f\"," +
                                             "\"timestamp\":1800000000000,\"signature\":\"MEUCIQD0ZXN0\"," +
                                             "\"app_account_token\":\"c3214538-d133-89d9-b4db-63a435cddcae\",\"signed_offer_jws\":null}"),
            };
            var api = new BillingApiClient("https://b.example.com/route/", 10, transport);

            var result = Sync.Run(api.SignApplePromotionalOfferAsync(new ApplePromotionalOfferRequest
            {
                BundleId = "com.test.game.ios", ProductId = "premium_ios", OfferId = "winback_50", UserId = User,
            }));

            Assert.AreEqual("https://b.example.com/route/offers/apple/sign", transport.Requests[0].Url);
            var body = JObject.Parse(transport.Requests[0].Body);
            Assert.AreEqual("com.test.game.ios", (string)body["bundle_id"]);
            Assert.AreEqual("winback_50", (string)body["offer_id"]);
            Assert.IsNull(body["transaction_id"]);
            var offer = result.Value.ToOffer();
            Assert.IsTrue(offer.IsWellFormed(out var problem), problem);
            Assert.IsNull(offer.SignedJws);
            // Shared test vector with the server: apple_account_token("player-42").
            Assert.AreEqual(AccountIds.AppleAccountToken(User).ToString(), offer.AppAccountToken);
        }
    }
}
