using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Google Play and Xsolla in one build (<see cref="CompositeStoreAdapter"/>), moving the old Xsolla
    /// inventory, app link returns, child-directed apps.</summary>
    public sealed class MultiStoreTests
    {
        private BillingConfig m_Config;
        private XsollaServer m_Server;
        private FakeStore m_Google;
        private FakeApi m_Api;
        private DateTime m_Now;
        private XsollaStoreAdapter m_Xsolla;
        private BillingService m_Service;
        private string m_Route;
        private readonly List<string> m_Log = new List<string>();

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable));
            m_Config.WithBuild(BillingStores.Google, "com.test.game").WithBuild(BillingStores.Xsolla, "com.test.game.xsolla");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.Warning;
            // NUnit reuses the fixture: every field a test changes starts over here.
            m_Route = null;
            m_Log.Clear();
            m_Server = new XsollaServer();
            m_Google = new FakeStore();
            m_Api = new FakeApi();
            m_Now = new DateTime(2026, 11, 24, 12, 0, 0, DateTimeKind.Utc);
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService Start(XsollaOptions options = null)
        {
            options = options ?? new XsollaOptions();
            options.OpenUrl = url => { };
            m_Xsolla = new XsollaStoreAdapter(m_Config, options, m_Server, new MemoryBillingStorage())
            {
                Delay = (d, ct) =>
                {
                    m_Now += d;
                    return Task.CompletedTask;
                },
                UtcNow = () => m_Now,
            };
            var store = new CompositeStoreAdapter(m_Google, new FuncStoreRouter((product, loaded) => m_Route), m_Xsolla);
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = store,
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider("player-1"),
                Storage = new MemoryBillingStorage(),
                CreateLifecycleHook = false,
                IsEditor = false,
                Delay = (d, ct) => Task.CompletedTask,
                LogSink = (l, m) => m_Log.Add(m),
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void Composite_SendsRoutedPurchasesToXsollaAndTheRestToTheMainStore()
        {
            Start();
            m_Google.OnPurchase = id => FakeStore.GoogleOrder(id, "tok-g", "GPA.g");
            var google = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Success, google.Status, "google: " + google.Message + " | " + string.Join(" | ", m_Log) +
                " | xsolla: " + string.Join(", ", m_Server.Requests.Select(r => r.Url)));
            Assert.AreEqual((BillingStores.Google, "com.test.game"), (m_Api.VerifyStores.Single(), m_Api.VerifyRequests.Single().AppId));
            Assert.AreEqual(BillingStores.Google, m_Service.Store);

            m_Route = BillingStores.Xsolla;
            m_Server.Statuses.Add(XsollaServer.Status("success", delivery: "server"));
            var xsolla = Sync.Run(m_Service.PurchaseAsync("no_ads"));

            Assert.AreEqual(PurchaseStatus.Success, xsolla.Status, xsolla.Message);
            var checkout = m_Server.Requests.First(r => r.Url.EndsWith("/checkout/xsolla")).Body;
            Assert.AreEqual(("com.test.game.xsolla", "no_ads"), ((string)checkout["app_id"], (string)checkout["product_id"]));
            Assert.AreEqual(1, m_Api.VerifyRequests.Count, "Xsolla orders are checked through their own route");
            Assert.AreEqual(1, m_Google.PurchaseCalls.Count);
        }

        [Test]
        public void Restore_SendsEachStoresPurchasesToItsOwnBuild()
        {
            m_Config.WithBuild(BillingStores.Amazon, "com.test.game.amazon");
            var amazon = new FakeStore { Platform = BillingPlatform.Amazon };
            m_Google.Owned.Add(FakeStore.GoogleOrder("no_ads", token: "google-token"));
            amazon.Owned.Add(new StoreOrder(BillingPlatform.Amazon, "no_ads", "amzn-receipt-1", "amzn-receipt-1", storeUserId: "amzn-user"));
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = new CompositeStoreAdapter(m_Google, new FuncStoreRouter((product, loaded) => null), amazon),
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider("player-1"),
                Storage = new MemoryBillingStorage(),
                CreateLifecycleHook = false,
                IsEditor = false,
                Delay = (d, ct) => Task.CompletedTask,
            });
            Sync.Run(m_Service.InitializeAsync());
            m_Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse { CustomerInfo = FakeApi.Info(null, null) }, null);

            Sync.Run(m_Service.RestoreAsync());

            var byApp = m_Api.RestoreRequests.ToDictionary(r => r.AppId, r => r.Purchases.Single().PurchaseToken);
            CollectionAssert.AreEquivalent(new[] { "com.test.game", "com.test.game.amazon" }, byApp.Keys);
            Assert.AreEqual("google-token", byApp["com.test.game"]);
            Assert.AreEqual("amzn-receipt-1", byApp["com.test.game.amazon"]);
        }

        [Test]
        public void CurrencyRouter_PicksXsollaForRublePrices()
        {
            var router = new CurrencyStoreRouter(BillingStores.Xsolla, "rub");
            var config = new BillingProductConfig("gems_100", BillingProductType.Consumable);
            StoreProduct Loaded(string currency, bool fallback = false) =>
                new StoreProduct("gems_100", "gems_100", BillingProductType.Consumable, "99 ₽", 99m, currency, "", "", true, isFallbackPrice: fallback);
            Assert.AreEqual(BillingStores.Xsolla, router.StoreFor(config, Loaded("RUB")));
            Assert.IsNull(router.StoreFor(config, Loaded("USD")));
            Assert.IsNull(router.StoreFor(config, Loaded("RUB", fallback: true)), "a fallback price says nothing about the player");
            Assert.IsNull(router.StoreFor(config, null));
        }

        [Test]
        public void ClaimXsollaPurchases_PostsTheLoginTokenAndRefreshesTheCustomerInfo()
        {
            Start();
            m_Server.Statuses.Add(new HttpResponse(200,
                "{\"status\":\"ok\",\"claimed\":[{\"product_id\":\"no_ads\",\"purchase_status\":\"completed\"}],\"skipped\":[]}"));
            m_Api.EntitlementsResult = FakeApi.Entitlements(new Dictionary<string, long>(), "no_ads");

            var claimed = Sync.Run(m_Service.ClaimXsollaPurchasesAsync("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1MSJ9.c2ln"));

            Assert.AreEqual("no_ads", claimed.Claimed.Single().ProductId);
            var body = m_Server.Requests.Single(r => r.Url.EndsWith("/claim/xsolla")).Body;
            Assert.AreEqual(("com.test.game.xsolla", "player-1"), ((string)body["app_id"], (string)body["user_id"]));
            Assert.AreEqual(1, m_Api.EntitlementsCalls);
        }

        [Test]
        public void ClaimXsollaPurchases_ReportsARefusedToken()
        {
            Start();
            m_Server.Statuses.Add(new HttpResponse(400, "{\"status\":\"invalid\",\"message\":\"[ERR_XSOLLA_TOKEN] expired\"}"));
            var e = Assert.Throws<BillingException>(() => Sync.Run(m_Service.ClaimXsollaPurchasesAsync("a.b.c")));
            StringAssert.Contains("ERR_XSOLLA_TOKEN", e.Message);
        }

        [Test]
        public void AppLinkReturn_ChecksTheOrderAtOnce()
        {
            Assert.AreEqual("mygame://", XsollaStoreAdapter.AppLinkPrefix("mygame://purchase"));
            Assert.IsNull(XsollaStoreAdapter.AppLinkPrefix("https://example.com/back"));
            Assert.IsNull(XsollaStoreAdapter.AppLinkPrefix(null));

            using (Sync.InlineContinuations())
            {
            Start(new XsollaOptions { ReturnUrl = "mygame://purchase" });
            var waits = 0;
            var wake = new TaskCompletionSource<bool>();
            m_Xsolla.Delay = (d, ct) =>
            {
                waits++;
                return wake.Task; // the poll interval never ends by itself
            };
            m_Route = BillingStores.Xsolla;
            m_Server.Statuses.Add(XsollaServer.Status("pending_or_canceled"));
            m_Server.Statuses.Add(XsollaServer.Status("success", delivery: "server"));
            var purchase = m_Service.PurchaseAsync("no_ads");
            Assert.IsFalse(purchase.IsCompleted, "waiting for the next poll");
            Assert.AreEqual(1, waits);

            m_Xsolla.OnDeepLink("mygame://purchase?status=done");

            Assert.IsTrue(purchase.IsCompleted, "the app link woke the poll");
            Assert.AreEqual(PurchaseStatus.Success, purchase.Result.Status);
            }
        }

        [Test]
        public void ChildDirectedApps_SendNoAdvertisingIds()
        {
            using (var h = new ServiceHarness())
            {
                h.Config.childDirected = true;
                h.Attribution = new FuncAttributionProvider(() =>
                    new Dictionary<string, string> { [AttributionKeys.TenjinId] = "t-1", [AttributionKeys.AdvertisingId] = "gaid", [AttributionKeys.Idfa] = "idfa" });
                h.Start();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);
                Sync.Run(h.Service.PurchaseAsync("gems_100"));
                CollectionAssert.AreEquivalent(new[] { AttributionKeys.TenjinId }, h.Api.VerifyRequests.Single().Attribution.Keys);
            }
        }
    }
}
