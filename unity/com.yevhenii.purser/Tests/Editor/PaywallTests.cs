using System;
using System.Linq;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    public sealed class PaywallTests
    {
        private const string User = "player-1";
        private static readonly DateTime Now = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);

        // Server answer for an Android build: premium_android is the configured "premium",
        // "new_pack" is not in BillingConfig yet.
        private const string Answer = @"{
            ""status"": ""ok"", ""placement"": ""main_paywall"", ""paywall_id"": ""pw_sale"", ""paywall_name"": ""Sale"",
            ""variant_id"": ""b"", ""test_id"": ""main-ab-1"",
            ""products"": [
                {""id"": ""premium_android"", ""store_id"": ""premium_android"", ""kind"": ""subscription""},
                {""id"": ""new_pack"", ""store_id"": ""new_pack"", ""kind"": ""consumable""},
                {""id"": ""gems_100"", ""store_id"": ""gems_100"", ""kind"": ""consumable""}
            ],
            ""remote_config"": {""title"": ""Sale"", ""discount"": 30, ""colors"": {""accent"": ""#ff0000""}},
            ""updated_at_ms"": 1800000000000
        }";

        private BillingConfig m_Config;
        private FakeStore m_Store;
        private FakeApi m_Api;
        private MemoryBillingStorage m_Storage;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("premium", BillingProductType.Subscription)
                    .WithStoreId(BillingStores.Google, "premium_android").WithStoreId(BillingStores.Apple, "premium_ios"));
            m_Config.WithBuild(BillingStores.Google, "com.example.mygame").WithBuild(BillingStores.Apple, "com.example.mygame.ios");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Store = new FakeStore();
            m_Api = new FakeApi();
            m_Storage = new MemoryBillingStorage();
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService CreateService(bool isEditor = false)
        {
            m_Service?.Dispose();
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = m_Store,
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider(User),
                Storage = m_Storage,
                UtcNow = () => Now,
                Delay = (delay, ct) => Task.CompletedTask,
                IsEditor = isEditor,
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void GetPaywall_MapsProductsToConfigAndStorePrices()
        {
            CreateService();
            m_Api.PaywallResult = FakeApi.Paywall(Answer);

            var paywall = Sync.Run(m_Service.GetPaywallAsync("main_paywall"));

            var request = m_Api.PaywallRequests.Single();
            Assert.AreEqual("com.example.mygame", request.AppId);
            Assert.AreEqual("main_paywall", request.Placement);
            Assert.AreEqual(User, request.UserId);

            Assert.AreEqual("pw_sale", paywall.PaywallId);
            Assert.AreEqual("b", paywall.VariantId);
            Assert.AreEqual("main-ab-1", paywall.TestId);
            Assert.IsTrue(paywall.IsInTest);
            Assert.IsFalse(paywall.IsFromCache);
            CollectionAssert.AreEqual(new[] { "premium", "gems_100" }, paywall.Products.Select(p => p.Id).ToArray());
            Assert.AreEqual("premium_android", paywall.Products[0].StoreId);
            Assert.AreEqual(BillingProductType.Subscription, paywall.Products[0].Type);
            Assert.AreEqual("$0.99", paywall.Products[0].LocalizedPrice);
            Assert.IsTrue(paywall.Products[1].AvailableToPurchase);
            CollectionAssert.AreEqual(new[] { "new_pack" }, paywall.MissingProductIds.ToArray());
        }

        [Test]
        public void RemoteConfig_HelpersReadTheJson()
        {
            CreateService();
            m_Api.PaywallResult = FakeApi.Paywall(Answer);
            var paywall = Sync.Run(m_Service.GetPaywallAsync("main_paywall"));

            Assert.AreEqual("Sale", paywall.GetValue<string>("title"));
            Assert.AreEqual(30, paywall.GetValue<int>("discount"));
            Assert.AreEqual("fallback", paywall.GetValue("missing", "fallback"));
            Assert.AreEqual("#ff0000", (string)paywall.RemoteConfigObject["colors"]["accent"]);
            Assert.AreEqual(30, paywall.GetRemoteConfig<SaleConfig>().Discount);
            StringAssert.Contains("\"discount\":30", paywall.RemoteConfigJson);

            var empty = new Paywall("p", "pw", "n", "a", "", null, null, null, false, Now);
            Assert.AreEqual("{}", empty.RemoteConfigJson);
            Assert.AreEqual(0, empty.RemoteConfigObject.Count);
            var broken = new Paywall("p", "pw", "n", "a", "", null, null, "{oops", false, Now);
            Assert.IsNull(broken.RemoteConfig);
            Assert.AreEqual(5, broken.GetValue("x", 5));
        }

        private sealed class SaleConfig
        {
            public string Title;
            public int Discount;
        }

        [Test]
        public void GetPaywall_Offline_UsesTheCachedAnswer()
        {
            CreateService();
            m_Api.PaywallResult = FakeApi.Paywall(Answer);
            Sync.Run(m_Service.GetPaywallAsync("main_paywall"));

            // New session without network: the cache in storage answers.
            CreateService();
            m_Api.PaywallResult = new ApiResult<PaywallResponse>(0, null, "no network");
            var cached = Sync.Run(m_Service.GetPaywallAsync("main_paywall"));
            Assert.IsTrue(cached.IsFromCache);
            Assert.AreEqual("b", cached.VariantId);
            Assert.AreEqual(Now, cached.FetchedAtUtc);
            Assert.AreEqual(2, cached.Products.Count);

            // Nothing cached for another placement.
            var e = Assert.Throws<BillingException>(() => Sync.Run(m_Service.GetPaywallAsync("shop")));
            StringAssert.Contains("unavailable", e.Message);
        }

        [Test]
        public void GetPaywall_UnknownPlacement_ThrowsAndForgetsTheCache()
        {
            CreateService();
            m_Api.PaywallResult = FakeApi.Paywall(Answer);
            Sync.Run(m_Service.GetPaywallAsync("main_paywall"));

            m_Api.PaywallResult = new ApiResult<PaywallResponse>(404,
                new PaywallResponse { Status = "not_found", Code = "ERR_PLACEMENT_NOT_FOUND", Message = "no placement" }, "HTTP 404");
            var e = Assert.Throws<PaywallNotFoundException>(() => Sync.Run(m_Service.GetPaywallAsync("main_paywall")));
            Assert.AreEqual("ERR_PLACEMENT_NOT_FOUND", e.Code);
            Assert.AreEqual("main_paywall", e.PlacementId);

            m_Api.PaywallResult = new ApiResult<PaywallResponse>(0, null, "no network");
            Assert.Throws<BillingException>(() => Sync.Run(m_Service.GetPaywallAsync("main_paywall")));
        }

        [Test]
        public void PurchaseFromPaywall_SendsTheVariantWithVerify()
        {
            CreateService();
            m_Api.PaywallResult = FakeApi.Paywall(Answer);
            var paywall = Sync.Run(m_Service.GetPaywallAsync("main_paywall"));

            m_Store.OnPurchase = id => FakeStore.GoogleOrder(id);
            var result = Sync.Run(m_Service.PurchaseAsync(paywall.Products[1].Id, paywall));
            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual("main_paywall", request.Placement);
            Assert.AreEqual("b", request.VariantId);
            Assert.AreEqual("main-ab-1", request.TestId);
            Assert.AreEqual("pw_sale", request.PaywallId);

            // A purchase without a paywall carries nothing.
            m_Store.OnPurchase = id => FakeStore.GoogleOrder(id, "token-2", "GPA.2");
            Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.IsNull(m_Api.VerifyRequests[1].Placement);
            Assert.IsNull(m_Api.VerifyRequests[1].VariantId);
        }

        [Test]
        public void LogPaywallShown_PostsAView()
        {
            CreateService();
            m_Api.PaywallResult = FakeApi.Paywall(Answer);
            var paywall = Sync.Run(m_Service.GetPaywallAsync("main_paywall"));

            Assert.IsTrue(Sync.Run(m_Service.LogPaywallShownAsync(paywall)));
            var ev = m_Api.PaywallEvents.Single();
            Assert.AreEqual(("com.example.mygame", "main_paywall", "b", "main-ab-1", User, "view"),
                (ev.AppId, ev.Placement, ev.VariantId, ev.TestId, ev.UserId, ev.Event));

            m_Api.PaywallEventResult = new ApiResult<StatusResponse>(0, null, "no network");
            Assert.IsFalse(Sync.Run(m_Service.LogPaywallShownAsync(paywall)));
            Assert.IsFalse(Sync.Run(m_Service.LogPaywallShownAsync(null)));
        }

        [Test]
        public void LogPaywallShown_IsSkippedInTheEditorWithoutServer()
        {
            CreateService(isEditor: true);
            m_Api.PaywallResult = FakeApi.Paywall(Answer);
            var paywall = Sync.Run(m_Service.GetPaywallAsync("main_paywall"));
            Assert.IsFalse(Sync.Run(m_Service.LogPaywallShownAsync(paywall)));
            Assert.AreEqual(0, m_Api.PaywallEvents.Count);
        }

        [Test]
        public void ApiClient_PostsToThePaywallRoutes()
        {
            var transport = new FakeTransport { Next = new HttpResponse(200, Answer) };
            var api = new BillingApiClient("https://billing.example.com/route/", 10, transport);

            var result = Sync.Run(api.PaywallAsync(new PaywallRequest { AppId = "com.example.mygame", Placement = "main_paywall" }));
            Assert.AreEqual("https://billing.example.com/route/paywall", transport.Requests[0].Url);
            Assert.AreEqual("{\"app_id\":\"com.example.mygame\",\"placement\":\"main_paywall\"}", transport.Requests[0].Body);
            Assert.AreEqual("main-ab-1", result.Value.TestId);
            Assert.AreEqual(3, result.Value.Products.Count);

            transport.Next = new HttpResponse(200, "{\"status\":\"ok\"}");
            Sync.Run(api.PaywallEventAsync(new PaywallEventRequest { AppId = "a", Placement = "p", VariantId = "b" }));
            Assert.AreEqual("https://billing.example.com/route/paywall/event", transport.Requests[1].Url);
            Assert.AreEqual("{\"app_id\":\"a\",\"placement\":\"p\",\"variant_id\":\"b\",\"event\":\"view\"}", transport.Requests[1].Body);
        }
    }
}
