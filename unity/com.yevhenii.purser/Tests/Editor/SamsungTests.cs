using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Samsung IAP stand-in: answers synchronously.</summary>
    internal sealed class FakeSamsungIap : ISamsungIap
    {
        public bool Available = true;
        public readonly List<SamsungPurchase> Owned = new List<SamsungPurchase>();
        public readonly List<string> Consumed = new List<string>();
        public readonly List<string> Purchases = new List<string>();
        public Func<string, SamsungPurchaseResult> OnPurchase;
        public bool FailConsume;

        public Task InitializeAsync(CancellationToken ct) =>
            Available ? Task.CompletedTask : Task.FromException(new BillingException("Samsung IAP is not available"));

        public Task<IReadOnlyList<SamsungProduct>> GetProductsAsync(IReadOnlyList<string> itemIds, CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<SamsungProduct>>(itemIds.Select(id => new SamsungProduct
            {
                ItemId = id, PriceString = "€1.99", Price = 1.99m, Currency = "EUR", Name = id,
                SubscriptionPeriod = id.StartsWith("vip") ? "P1M" : null,
            }).ToList());

        public Task<SamsungPurchaseResult> PurchaseAsync(string itemId, CancellationToken ct)
        {
            Purchases.Add(itemId);
            return Task.FromResult(OnPurchase?.Invoke(itemId) ?? new SamsungPurchaseResult
            {
                ErrorCode = SamsungStatus.Success, Purchase = Purchase(itemId, "order-" + itemId),
            });
        }

        public Task<IReadOnlyList<SamsungPurchase>> GetOwnedPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<SamsungPurchase>>(Owned.ToList());

        public Task ConsumeAsync(string purchaseId, CancellationToken ct)
        {
            if (FailConsume)
                return Task.FromException(new BillingException("consume failed"));
            Consumed.Add(purchaseId);
            return Task.CompletedTask;
        }

        public static SamsungPurchase Purchase(string itemId, string orderId) => new SamsungPurchase
        {
            PurchaseId = "purchase-" + orderId, OrderId = orderId, ItemId = itemId,
        };
    }

    public sealed class SamsungStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private FakeSamsungIap m_Iap;
        private FakeApi m_Api;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable),
                new BillingProductConfig("vip", BillingProductType.Subscription).WithStoreId(BillingStores.Samsung, "vip_monthly"));
            m_Config.WithBuild(BillingStores.Samsung, "com.test.game.samsung");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Iap = new FakeSamsungIap();
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
                Store = new SamsungStoreAdapter(m_Iap),
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
        public void Consumable_IsVerifiedOnTheSamsungRouteByItsPurchaseIdThenConsumed()
        {
            CreateService();
            Assert.AreEqual(BillingPlatform.Samsung, m_Service.Platform);
            var gems = m_Service.GetProduct("gems_100");
            Assert.AreEqual((1.99m, "EUR"), (gems.Price, gems.Currency));

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            Assert.AreEqual("gems_100", m_Iap.Purchases.Single());
            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual(("samsung", "com.test.game.samsung"), (m_Api.VerifyStores.Single(), request.AppId));
            Assert.AreEqual(("purchase-order-gems_100", "order-gems_100"), (request.PurchaseToken, request.OrderId));
            CollectionAssert.AreEqual(new[] { "purchase-order-gems_100" }, m_Iap.Consumed);
        }

        [Test]
        public void SubscriptionsAndNonConsumables_AreNotConsumed()
        {
            CreateService();
            Assert.AreEqual("P1M", m_Service.GetProduct("vip").SubscriptionPeriod);

            Assert.AreEqual(PurchaseStatus.Success, Sync.Run(m_Service.PurchaseAsync("vip")).Status);
            Assert.AreEqual(PurchaseStatus.Success, Sync.Run(m_Service.PurchaseAsync("no_ads")).Status);

            CollectionAssert.AreEqual(new[] { "vip_monthly", "no_ads" }, m_Iap.Purchases);
            Assert.AreEqual("purchase-order-vip_monthly", m_Api.VerifyRequests[0].PurchaseToken);
            Assert.IsEmpty(m_Iap.Consumed, "the server acknowledges them");
        }

        [Test]
        public void ARejectedConsumable_IsNotConsumed()
        {
            m_Api.DefaultVerify = FakeApi.Verify(400, VerifyStatus.Invalid, "not exist order");
            CreateService();

            Assert.AreEqual(PurchaseStatus.Rejected, Sync.Run(m_Service.PurchaseAsync("gems_100")).Status);
            Assert.IsEmpty(m_Iap.Consumed, "verified again at the next launch");
        }

        [Test]
        public void CancelledAndOwnedPurchasesFailWithoutVerification()
        {
            CreateService();
            m_Iap.OnPurchase = id => new SamsungPurchaseResult { ErrorCode = SamsungStatus.Cancelled };
            Assert.AreEqual(PurchaseStatus.Cancelled, Sync.Run(m_Service.PurchaseAsync("gems_100")).Status);
            m_Iap.OnPurchase = id => new SamsungPurchaseResult { ErrorCode = SamsungStatus.AlreadyPurchased };
            var owned = Sync.Run(m_Service.PurchaseAsync("no_ads"));
            Assert.AreNotEqual(PurchaseStatus.Success, owned.Status);
            StringAssert.Contains("already owned", owned.Message);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void UnconsumedConsumablesAreVerifiedOnConnect_RestoreSendsEveryOwnedOne()
        {
            // Owned purchases carry no order id; only the consumable is a purchase whose answer never came.
            m_Iap.Owned.Add(new SamsungPurchase { PurchaseId = "p-1", ItemId = "gems_100" });
            m_Iap.Owned.Add(new SamsungPurchase { PurchaseId = "p-2", ItemId = "no_ads" });
            CreateService();
            Assert.AreEqual("p-1", m_Api.VerifyRequests.Single().PurchaseToken);
            CollectionAssert.AreEqual(new[] { "p-1" }, m_Iap.Consumed);

            m_Iap.Owned.RemoveAt(0);
            m_Iap.Owned.Add(new SamsungPurchase { PurchaseId = "p-3", ItemId = "vip_monthly" });
            m_Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse { CustomerInfo = FakeApi.Info(null, null) }, null);

            Sync.Run(m_Service.RestoreAsync());

            var items = m_Api.RestoreRequests.Single().Purchases.Select(p => (p.ProductId, p.PurchaseToken)).ToList();
            CollectionAssert.AreEquivalent(new[] { ("no_ads", "p-2"), ("vip_monthly", "p-3") }, items);
            Assert.AreEqual("com.test.game.samsung", m_Api.RestoreRequests.Single().AppId);
        }

        [Test]
        public void AFailedConsumeIsReported()
        {
            m_Iap.FailConsume = true;
            var finished = new List<(StoreOrder, bool, string)>();
            var adapter = new SamsungStoreAdapter(m_Iap);
            Sync.Run(adapter.FetchProductsAsync(m_Config.products, CancellationToken.None));
            adapter.OrderFinished += (o, ok, e) => finished.Add((o, ok, e));
            var order = adapter.Map(FakeSamsungIap.Purchase("gems_100", "o-9"));

            adapter.Finish(order, true);

            Assert.IsFalse(finished.Single().Item2);
            StringAssert.Contains("consume failed", finished.Single().Item3);
        }

        [Test]
        public void SamsungIsAStoreOfItsOwn()
        {
            Assert.AreEqual(BillingStores.Samsung, BillingStores.Of(BillingPlatform.Samsung));
            CollectionAssert.Contains(BillingStores.All, BillingStores.Samsung);
        }
    }
}
