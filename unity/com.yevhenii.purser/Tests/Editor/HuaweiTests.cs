using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>HMS IAP stand-in: answers synchronously.</summary>
    internal sealed class FakeHuaweiIap : IHuaweiIap
    {
        public bool Available = true;
        public readonly Dictionary<HuaweiPriceType, List<HuaweiPurchase>> Owned = new Dictionary<HuaweiPriceType, List<HuaweiPurchase>>();
        public readonly List<string> Consumed = new List<string>();
        public readonly List<(string ProductId, HuaweiPriceType Type)> Purchases = new List<(string, HuaweiPriceType)>();
        public Func<string, HuaweiPriceType, HuaweiPurchaseResult> OnPurchase;
        public bool FailConsume;

        public Task InitializeAsync(CancellationToken ct) =>
            Available ? Task.CompletedTask : Task.FromException(new BillingException("HMS IAP is not available"));

        public Task<IReadOnlyList<HuaweiProduct>> GetProductsAsync(HuaweiPriceType type, IReadOnlyList<string> productIds, CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<HuaweiProduct>>(productIds.Select(id => new HuaweiProduct
            {
                ProductId = id, PriceType = type, Price = "€1.99", MicrosPrice = 1_990_000, Currency = "EUR", ProductName = id,
                SubPeriod = type == HuaweiPriceType.Subscription ? "P1M" : null,
            }).ToList());

        public Task<HuaweiPurchaseResult> PurchaseAsync(string productId, HuaweiPriceType type, string developerPayload, CancellationToken ct)
        {
            Purchases.Add((productId, type));
            return Task.FromResult(OnPurchase?.Invoke(productId, type) ?? new HuaweiPurchaseResult
            {
                ReturnCode = HuaweiStatus.Success, Purchase = Purchase(productId, type, "order-" + productId),
            });
        }

        public Task<IReadOnlyList<HuaweiPurchase>> GetOwnedPurchasesAsync(HuaweiPriceType type, CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<HuaweiPurchase>>(Owned.TryGetValue(type, out var list) ? list : new List<HuaweiPurchase>());

        public Task ConsumeAsync(string purchaseToken, CancellationToken ct)
        {
            if (FailConsume)
                return Task.FromException(new BillingException("consume failed"));
            Consumed.Add(purchaseToken);
            return Task.CompletedTask;
        }

        public static HuaweiPurchase Purchase(string productId, HuaweiPriceType type, string orderId) => new HuaweiPurchase
        {
            OrderId = orderId, PurchaseToken = "token-" + orderId, ProductId = productId, PriceType = type,
            SubscriptionId = type == HuaweiPriceType.Subscription ? "sub-" + productId : null,
        };
    }

    public sealed class HuaweiStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private FakeHuaweiIap m_Iap;
        private FakeApi m_Api;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable),
                new BillingProductConfig("vip", BillingProductType.Subscription).WithStoreId(BillingStores.Huawei, "vip_monthly"));
            m_Config.WithBuild(BillingStores.Huawei, "com.test.game.huawei");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.restoreOnFirstLaunch = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Iap = new FakeHuaweiIap();
            m_Api = new FakeApi();
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService CreateService()
        {
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = new HuaweiStoreAdapter(m_Iap),
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider(User),
                Storage = new MemoryBillingStorage(),
                Delay = (d, ct) => Task.CompletedTask,
                IsEditor = false,
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void Consumable_IsVerifiedOnTheHuaweiRouteThenConsumed()
        {
            CreateService();
            Assert.AreEqual(BillingPlatform.Huawei, m_Service.Platform);
            var gems = m_Service.GetProduct("gems_100");
            Assert.AreEqual((1.99m, "EUR"), (gems.Price, gems.Currency));

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            Assert.AreEqual(("gems_100", HuaweiPriceType.Consumable), m_Iap.Purchases.Single());
            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual(("huawei", "com.test.game.huawei"), (m_Api.VerifyStores.Single(), request.AppId));
            Assert.AreEqual(("token-order-gems_100", "order-gems_100"), (request.PurchaseToken, request.OrderId));
            CollectionAssert.AreEqual(new[] { "token-order-gems_100" }, m_Iap.Consumed);
        }

        [Test]
        public void Subscription_IsVerifiedByItsSubscriptionIdAndNotConsumed()
        {
            CreateService();
            Assert.AreEqual("P1M", m_Service.GetProduct("vip").SubscriptionPeriod);

            var result = Sync.Run(m_Service.PurchaseAsync("vip"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            Assert.AreEqual(("vip_monthly", HuaweiPriceType.Subscription), m_Iap.Purchases.Single());
            Assert.AreEqual("sub-vip_monthly", m_Api.VerifyRequests.Single().OrderId);
            Assert.IsEmpty(m_Iap.Consumed);
        }

        [Test]
        public void CancelledAndOwnedPurchasesFailWithoutVerification()
        {
            CreateService();
            m_Iap.OnPurchase = (id, type) => new HuaweiPurchaseResult { ReturnCode = HuaweiStatus.Cancelled };
            Assert.AreEqual(PurchaseStatus.Cancelled, Sync.Run(m_Service.PurchaseAsync("gems_100")).Status);
            m_Iap.OnPurchase = (id, type) => new HuaweiPurchaseResult { ReturnCode = HuaweiStatus.ProductOwned };
            var owned = Sync.Run(m_Service.PurchaseAsync("no_ads"));
            Assert.AreNotEqual(PurchaseStatus.Success, owned.Status);
            StringAssert.Contains("already owned", owned.Message);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void UnconsumedPurchasesAreVerifiedOnConnect_RestoreSendsEveryOwnedOne()
        {
            m_Iap.Owned[HuaweiPriceType.Consumable] = new List<HuaweiPurchase> { FakeHuaweiIap.Purchase("gems_100", HuaweiPriceType.Consumable, "o-1") };
            CreateService();
            Assert.AreEqual("o-1", m_Api.VerifyRequests.Single().OrderId, "a purchase whose answer never came");
            CollectionAssert.AreEqual(new[] { "token-o-1" }, m_Iap.Consumed);

            m_Iap.Owned[HuaweiPriceType.Consumable].Clear();
            m_Iap.Owned[HuaweiPriceType.NonConsumable] = new List<HuaweiPurchase> { FakeHuaweiIap.Purchase("no_ads", HuaweiPriceType.NonConsumable, "o-2") };
            m_Iap.Owned[HuaweiPriceType.Subscription] = new List<HuaweiPurchase> { FakeHuaweiIap.Purchase("vip_monthly", HuaweiPriceType.Subscription, "o-3") };
            m_Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse { CustomerInfo = FakeApi.Info(null, null) }, null);

            Sync.Run(m_Service.RestoreAsync());

            var items = m_Api.RestoreRequests.Single().Purchases.Select(p => (p.ProductId, p.OrderId, p.PurchaseToken)).ToList();
            CollectionAssert.AreEquivalent(new[] { ("no_ads", "o-2", "token-o-2"), ("vip_monthly", "sub-vip_monthly", "token-o-3") }, items);
            Assert.AreEqual("com.test.game.huawei", m_Api.RestoreRequests.Single().AppId);
        }

        [Test]
        public void AFailedConsumeIsReported()
        {
            m_Iap.FailConsume = true;
            CreateService();
            var finished = new List<(StoreOrder, bool, string)>();
            var adapter = new HuaweiStoreAdapter(m_Iap);
            adapter.OrderFinished += (o, ok, e) => finished.Add((o, ok, e));
            var order = adapter.Map(FakeHuaweiIap.Purchase("gems_100", HuaweiPriceType.Consumable, "o-9"));

            adapter.Finish(order, true);

            Assert.IsFalse(finished.Single().Item2);
            StringAssert.Contains("consume failed", finished.Single().Item3);
        }

        [Test]
        public void HuaweiIsAStoreOfItsOwn()
        {
            Assert.AreEqual(BillingStores.Huawei, BillingStores.Of(BillingPlatform.Huawei));
            CollectionAssert.Contains(BillingStores.All, BillingStores.Huawei);
            CollectionAssert.Contains(BillingStores.All, BillingStores.Telegram);
        }
    }
}
