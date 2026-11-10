using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Amazon Appstore SDK stand-in: answers synchronously like the fakes of the other tests.</summary>
    internal sealed class FakeAmazonIap : IAmazonIap
    {
        public event Action<AmazonUserDataResponse> UserDataReceived;
        public event Action<AmazonProductDataResponse> ProductDataReceived;
        public event Action<AmazonPurchaseResponse> PurchaseReceived;
        public event Action<AmazonPurchaseUpdatesResponse> PurchaseUpdatesReceived;

        public string UserId = "amzn1.account.TEST";
        /// <summary>Pages returned by GetPurchaseUpdates, in order (the last one has HasMore = false).</summary>
        public readonly List<List<AmazonReceipt>> UpdatePages = new List<List<AmazonReceipt>>();
        public readonly List<bool> UpdateResets = new List<bool>();
        public readonly List<(string ReceiptId, bool Fulfilled)> Fulfillments = new List<(string, bool)>();
        public Func<string, AmazonPurchaseResponse> OnPurchase;
        private int m_Page;

        public void GetUserData() =>
            UserDataReceived?.Invoke(new AmazonUserDataResponse { Status = AmazonStatus.Successful, UserId = UserId, Marketplace = "US" });

        public void GetProductData(IReadOnlyList<string> skus) =>
            ProductDataReceived?.Invoke(new AmazonProductDataResponse
            {
                Status = AmazonStatus.Successful,
                Products = skus.Select(s => new AmazonProduct { Sku = s, ProductType = AmazonProductType.Consumable, Price = "$0.99", Title = s }).ToList(),
            });

        public void Purchase(string sku) => PurchaseReceived?.Invoke(OnPurchase?.Invoke(sku)
            ?? new AmazonPurchaseResponse { Status = AmazonStatus.Successful, UserId = UserId, Receipt = Receipt(sku, "receipt-" + sku) });

        public void GetPurchaseUpdates(bool reset)
        {
            UpdateResets.Add(reset);
            if (reset)
                m_Page = 0;
            var page = m_Page < UpdatePages.Count ? UpdatePages[m_Page] : new List<AmazonReceipt>();
            m_Page++;
            PurchaseUpdatesReceived?.Invoke(new AmazonPurchaseUpdatesResponse
            {
                Status = AmazonStatus.Successful, UserId = UserId, Receipts = page, HasMore = m_Page < UpdatePages.Count,
            });
        }

        public void NotifyFulfillment(string receiptId, bool fulfilled) => Fulfillments.Add((receiptId, fulfilled));

        public static AmazonReceipt Receipt(string sku, string receiptId, long cancelDate = 0) =>
            new AmazonReceipt { ReceiptId = receiptId, Sku = sku, ProductType = AmazonProductType.Consumable, PurchaseDate = 1, CancelDate = cancelDate };
    }

    public sealed class AmazonStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private FakeAmazonIap m_Iap;
        private MemoryBillingStorage m_Storage;
        private FakeApi m_Api;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("vip", BillingProductType.Subscription, "vip_android", "vip_ios", "vip_monthly"));
            m_Config.amazonAppId = "com.test.game.amazon";
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Iap = new FakeAmazonIap();
            m_Storage = new MemoryBillingStorage();
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
                Store = new AmazonStoreAdapter(m_Iap, m_Storage),
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
        public void Purchase_IsVerifiedOnTheAmazonRouteThenFulfilled()
        {
            CreateService();
            Assert.AreEqual(BillingPlatform.Amazon, m_Service.Platform);
            Assert.AreEqual("vip_monthly", m_Service.GetProduct("vip").StoreId);

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            var request = m_Api.VerifyRequests.Single();
            Assert.AreEqual("amazon", request.Store);
            Assert.AreEqual(BillingPlatform.Amazon, m_Api.VerifyPlatforms.Single());
            Assert.AreEqual("com.test.game.amazon", request.BundleId);
            Assert.AreEqual("receipt-gems_100", request.OrderId);
            Assert.AreEqual("amzn1.account.TEST", request.StoreUserId);
            Assert.IsNull(request.PurchaseToken);
            Assert.AreEqual(("receipt-gems_100", true), m_Iap.Fulfillments.Single());
        }

        [Test]
        public void UnfulfilledReceiptsAreDeliveredOnConnect_FulfilledOnesAreRemembered()
        {
            m_Iap.UpdatePages.Add(new List<AmazonReceipt> { FakeAmazonIap.Receipt("gems_100", "r-1") });
            m_Iap.UpdatePages.Add(new List<AmazonReceipt> { FakeAmazonIap.Receipt("gems_100", "r-2"), FakeAmazonIap.Receipt("gems_100", "r-3", cancelDate: 5) });
            CreateService();

            CollectionAssert.AreEqual(new[] { "r-1", "r-2" }, m_Api.VerifyRequests.Select(r => r.OrderId).ToArray(), "canceled receipts are skipped");
            CollectionAssert.AreEqual(new[] { "r-1", "r-2" }, m_Iap.Fulfillments.Select(f => f.ReceiptId).ToArray());
            m_Service.Dispose();

            // Next launch: Amazon still lists them (entitlements, subscriptions); they are not verified again.
            m_Api = new FakeApi();
            m_Iap = new FakeAmazonIap();
            m_Iap.UpdatePages.Add(new List<AmazonReceipt> { FakeAmazonIap.Receipt("gems_100", "r-1"), FakeAmazonIap.Receipt("gems_100", "r-4") });
            CreateService();
            Assert.AreEqual("r-4", m_Api.VerifyRequests.Single().OrderId);
        }

        [Test]
        public void Restore_SendsEveryOwnedReceiptWithTheAmazonUser()
        {
            CreateService();
            m_Iap.UpdatePages.Add(new List<AmazonReceipt> { FakeAmazonIap.Receipt("vip_monthly", "sub-1") });
            m_Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse { Entitlements = new EntitlementsDto { Subscriptions = new Dictionary<string, long>(), NonConsumables = new List<string>() } }, null);

            Sync.Run(m_Service.RestoreAsync());

            Assert.IsTrue(m_Iap.UpdateResets.Last(), "restore asks Amazon for everything");
            var item = m_Api.RestoreRequests.Single().Purchases.Single();
            Assert.AreEqual(("vip_monthly", "sub-1", "amzn1.account.TEST"), (item.ProductId, item.OrderId, item.StoreUserId));
            Assert.AreEqual("com.test.game.amazon", m_Api.RestoreRequests.Single().AppId);
        }

        [Test]
        public void FailedPurchaseIsReported()
        {
            m_Iap.OnPurchase = sku => new AmazonPurchaseResponse { Status = AmazonStatus.AlreadyPurchased, Sku = sku };
            CreateService();
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            StringAssert.Contains("already owned", result.Message);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void FormattedPricesAreParsed()
        {
            Assert.AreEqual(0.99m, AmazonStoreAdapter.ParsePrice("$0.99"));
            Assert.AreEqual(1234.5m, AmazonStoreAdapter.ParsePrice("1.234,50 €"));
            Assert.AreEqual(1299m, AmazonStoreAdapter.ParsePrice("¥1,299"));
            Assert.AreEqual(0m, AmazonStoreAdapter.ParsePrice("free"));
            Assert.AreEqual(0m, AmazonStoreAdapter.ParsePrice(null));
        }
    }

    public sealed class StoreRoutingTests
    {
        [Test]
        public void ApiClientUsesTheRequestStoreOrThePlatformStore()
        {
            var transport = new FakeTransport { Next = new HttpResponse(200, "{\"status\":\"success\"}") };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);

            Sync.Run(api.VerifyAsync(BillingPlatform.Amazon, new VerifyRequest { OrderId = "r", StoreUserId = "u" }));
            Assert.AreEqual("https://b.example.com/route/verify/amazon", transport.Requests[0].Url);
            var body = JObject.Parse(transport.Requests[0].Body);
            Assert.AreEqual("u", (string)body["store_user_id"]);
            Assert.IsFalse(body.ContainsKey("Store"), "the route is not part of the body");

            Sync.Run(api.VerifyAsync(BillingPlatform.Web, new VerifyRequest { Store = "stripe" }));
            Assert.AreEqual("https://b.example.com/route/verify/stripe", transport.Requests[1].Url);

            var missing = Sync.Run(api.VerifyAsync(BillingPlatform.Web, new VerifyRequest()));
            Assert.IsTrue(missing.IsTransportError, "a web adapter must name its store");
            Assert.AreEqual(2, transport.Requests.Count);
        }

        [Test]
        public void PlatformsMapToServerStores()
        {
            Assert.AreEqual("google", BillingPlatform.Google.StoreName());
            Assert.AreEqual("apple", BillingPlatform.Apple.StoreName());
            Assert.AreEqual("amazon", BillingPlatform.Amazon.StoreName());
            Assert.AreEqual("steam", BillingPlatform.Steam.StoreName());
            Assert.IsNull(BillingPlatform.Web.StoreName());
            Assert.AreEqual("google", BillingPlatform.Unsupported.RouteName(), "compatibility");
        }

        [Test]
        public void ConfigCarriesAmazonIds()
        {
            var config = BillingConfig.Create("https://b.example.com/route");
            config.ApplyJson(@"{""androidAppId"": ""com.example.mygame"", ""amazonAppId"": ""com.example.mygame.amazon"",
                ""products"": [{""id"": ""vip"", ""type"": ""subscription"", ""amazonId"": ""vip_monthly""}]}");
            Assert.AreEqual("com.example.mygame.amazon", config.AppIdFor(BillingPlatform.Amazon));
            Assert.AreEqual("com.example.mygame", config.AppIdFor(BillingPlatform.Google));
            Assert.AreEqual("vip_monthly", config.products[0].StoreId(BillingPlatform.Amazon));
            Assert.AreEqual("vip", config.products[0].StoreId(BillingPlatform.Google));
            Assert.IsTrue(config.products[0].Matches("vip_monthly"));
            UnityEngine.Object.DestroyImmediate(config);
        }

        [Test]
        public void AdapterNamesItsStore()
        {
            var config = BillingConfig.Create("https://b.example.com/route", new BillingProductConfig("gems", BillingProductType.Consumable));
            config.logLevel = BillingLogLevel.None;
            config.refreshEntitlementsOnInit = false;
            var store = new WebStore();
            var api = new FakeApi();
            var service = new BillingService(config, new BillingDependencies
            {
                Store = store, Api = api, UserIdProvider = new StaticUserIdProvider("u"), Storage = new MemoryBillingStorage(), IsEditor = false,
            });
            Sync.Run(service.InitializeAsync());
            store.OnPurchase = id => new StoreOrder(BillingPlatform.Web, id, "cs_test_1", null);
            Sync.Run(service.PurchaseAsync("gems"));
            Assert.AreEqual("stripe", api.VerifyRequests.Single().Store);
            service.Dispose();
            UnityEngine.Object.DestroyImmediate(config);
        }

        /// <summary>A game-provided web store adapter.</summary>
        private sealed class WebStore : IStoreAdapter, IStoreNameProvider
        {
            private readonly FakeStore m_Inner = new FakeStore { Platform = BillingPlatform.Web };
            public Func<string, StoreOrder> OnPurchase { set => m_Inner.OnPurchase = value; }
            public string StoreName => "stripe";
            public BillingPlatform Platform => BillingPlatform.Web;
            public event Action<StoreOrder> OrderPending { add => m_Inner.OrderPending += value; remove => m_Inner.OrderPending -= value; }
            public event Action<StoreOrder> OrderDeferred { add => m_Inner.OrderDeferred += value; remove => m_Inner.OrderDeferred -= value; }
            public event Action<StorePurchaseFailure> PurchaseFailed { add => m_Inner.PurchaseFailed += value; remove => m_Inner.PurchaseFailed -= value; }
            public event Action<StoreOrder, bool, string> OrderConfirmed { add => m_Inner.OrderConfirmed += value; remove => m_Inner.OrderConfirmed -= value; }
            public Task ConnectAsync(System.Threading.CancellationToken ct) => m_Inner.ConnectAsync(ct);
            public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, System.Threading.CancellationToken ct) => m_Inner.FetchProductsAsync(products, ct);
            public void SetAccountBinding(string googleAccountId, Guid? appleAccountToken) { }
            public void Purchase(string storeProductId) => m_Inner.Purchase(storeProductId);
            public void Confirm(StoreOrder order) => m_Inner.Confirm(order);
            public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(System.Threading.CancellationToken ct) => m_Inner.FetchPurchasesAsync(ct);
            public Task<bool> RestoreTransactionsAsync(System.Threading.CancellationToken ct) => m_Inner.RestoreTransactionsAsync(ct);
            public void Dispose() => m_Inner.Dispose();
        }
    }
}
