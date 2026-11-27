using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Meta Platform SDK stand-in: answers synchronously.</summary>
    internal sealed class FakeMetaQuestIap : IMetaQuestIap
    {
        public const string User = "5550001";
        public readonly List<MetaQuestPurchase> Owned = new List<MetaQuestPurchase>();
        public readonly List<string> Consumed = new List<string>();
        public readonly List<string> Checkouts = new List<string>();
        public Func<string, MetaQuestPurchaseResult> OnPurchase;
        public int Proofs;
        public bool FailProof;

        public Task<string> InitializeAsync(CancellationToken ct) => Task.FromResult(User);

        public Task<string> GetUserProofAsync(CancellationToken ct)
        {
            if (FailProof)
                return Task.FromException<string>(new BillingException("no proof"));
            Proofs++;
            return Task.FromResult("proof-" + Proofs);
        }

        public Task<IReadOnlyList<MetaQuestProduct>> GetProductsAsync(IReadOnlyList<string> skus, CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<MetaQuestProduct>>(skus.Select(sku => new MetaQuestProduct
            {
                Sku = sku, Type = MetaQuestProductType.Consumable, Name = sku, FormattedPrice = "$1.99", AmountInHundredths = 199, Currency = "USD",
            }).ToList());

        public Task<MetaQuestPurchaseResult> PurchaseAsync(string sku, CancellationToken ct)
        {
            Checkouts.Add(sku);
            return Task.FromResult(OnPurchase?.Invoke(sku) ?? new MetaQuestPurchaseResult
            {
                Purchase = Purchase(sku, sku == "no_ads" ? MetaQuestProductType.Durable : MetaQuestProductType.Consumable, "9001"),
            });
        }

        public Task<IReadOnlyList<MetaQuestPurchase>> GetPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<MetaQuestPurchase>>(Owned.ToList());

        public Task ConsumeAsync(string sku, CancellationToken ct)
        {
            Consumed.Add(sku);
            return Task.CompletedTask;
        }

        public static MetaQuestPurchase Purchase(string sku, MetaQuestProductType type, string id) => new MetaQuestPurchase
        {
            Id = id, Sku = sku, Type = type, GrantTime = new DateTime(2026, 1, 2, 3, 4, 5, DateTimeKind.Utc),
        };
    }

    public sealed class MetaQuestStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private FakeMetaQuestIap m_Iap;
        private FakeApi m_Api;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable).WithStoreId(BillingStores.Meta, "gems"),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable));
            m_Config.WithBuild(BillingStores.Meta, "com.test.game.quest");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.restoreOnFirstLaunch = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Iap = new FakeMetaQuestIap();
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
                Store = new MetaQuestStoreAdapter(m_Iap),
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
        public void Consumable_IsVerifiedWithTheMetaUserAndAProofThenConsumed()
        {
            CreateService();
            Assert.AreEqual(BillingPlatform.MetaQuest, m_Service.Platform);
            var gems = m_Service.GetProduct("gems_100");
            Assert.AreEqual((1.99m, "USD", "$1.99"), (gems.Price, gems.Currency, gems.LocalizedPrice));

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            CollectionAssert.AreEqual(new[] { "gems" }, m_Iap.Checkouts);
            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual(("meta", "com.test.game.quest"), (m_Api.VerifyStores.Single(), request.AppId));
            Assert.AreEqual(("9001", FakeMetaQuestIap.User, "proof-1"), (request.OrderId, request.StoreUserId, request.StoreUserProof));
            Assert.IsNull(request.PurchaseToken);
            CollectionAssert.AreEqual(new[] { "gems" }, m_Iap.Consumed);
        }

        [Test]
        public void EveryVerifyAttemptGetsANewProof()
        {
            CreateService();
            m_Api.VerifyResults.Enqueue(FakeApi.TransportError());

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status, "the caller is answered after the first failure");
            Assert.AreEqual(2, m_Api.VerifyRequests.Count);
            Assert.AreEqual(2, m_Iap.Proofs, "a proof is good for one check");
            Assert.AreEqual("proof-2", m_Api.VerifyRequests.Last().StoreUserProof);
            CollectionAssert.AreEqual(new[] { "gems" }, m_Iap.Consumed, "consumed once the retry was accepted");
        }

        [Test]
        public void WithoutAProofTheRequestIsStillSent_TheServerAsksAgain()
        {
            CreateService();
            m_Iap.FailProof = true;
            m_Api.DefaultVerify = FakeApi.Verify(400, VerifyStatus.Error, "store_user_proof (Users.GetUserProof) is required");

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreNotEqual(PurchaseStatus.Success, result.Status);
            Assert.IsNull(m_Api.VerifyRequests.First().StoreUserProof);
            Assert.IsEmpty(m_Iap.Consumed, "nothing is consumed before the server accepted it");
        }

        [Test]
        public void CancelledCheckoutFailsWithoutVerification()
        {
            CreateService();
            m_Iap.OnPurchase = sku => new MetaQuestPurchaseResult { Cancelled = true };
            Assert.AreEqual(PurchaseStatus.Cancelled, Sync.Run(m_Service.PurchaseAsync("gems_100")).Status);
            m_Iap.OnPurchase = sku => new MetaQuestPurchaseResult { Error = "network" };
            var failed = Sync.Run(m_Service.PurchaseAsync("no_ads"));
            Assert.AreNotEqual(PurchaseStatus.Success, failed.Status);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void UnconsumedPurchasesAreVerifiedOnConnect_RestoreSendsOneProofForEveryOwnedOne()
        {
            m_Iap.Owned.Add(FakeMetaQuestIap.Purchase("gems", MetaQuestProductType.Consumable, "9002"));
            m_Iap.Owned.Add(FakeMetaQuestIap.Purchase("no_ads", MetaQuestProductType.Durable, "9003"));
            CreateService();
            Assert.AreEqual("9002", m_Api.VerifyRequests.Single().OrderId, "only the consumable whose answer never came");

            m_Iap.Owned.RemoveAt(0);
            m_Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse { CustomerInfo = FakeApi.Info(null, null) }, null);
            var proofs = m_Iap.Proofs;

            Sync.Run(m_Service.RestoreAsync());

            var restore = m_Api.RestoreRequests.Single();
            Assert.AreEqual(("com.test.game.quest", "proof-" + (proofs + 1)), (restore.AppId, restore.StoreUserProof));
            var item = restore.Purchases.Single();
            Assert.AreEqual(("no_ads", "9003", FakeMetaQuestIap.User), (item.ProductId, item.OrderId, item.StoreUserId));
        }

        [Test]
        public void ASharedEntitlementIsKeyedBySkuAndGrantTime()
        {
            var adapter = new MetaQuestStoreAdapter(m_Iap);
            Sync.Run(adapter.ConnectAsync(CancellationToken.None));
            var order = adapter.Map(FakeMetaQuestIap.Purchase("no_ads", MetaQuestProductType.Durable, "0"));
            Assert.AreEqual("no_ads." + new DateTimeOffset(2026, 1, 2, 3, 4, 5, TimeSpan.Zero).ToUnixTimeSeconds(), order.TransactionId);
            Assert.AreEqual(BillingStores.Meta, BillingStores.Of(BillingPlatform.MetaQuest));
            CollectionAssert.Contains(BillingStores.All, BillingStores.Meta);
            adapter.Dispose();
        }
    }
}
