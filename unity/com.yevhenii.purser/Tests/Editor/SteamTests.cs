using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    internal sealed class FakeSteamBridge : ISteamMicroTxnBridge
    {
        public bool IsReady { get; set; } = true;
        public ulong SteamId { get; set; } = 76561197960287930UL;
        public string Language { get; set; } = "german";
        public event Action<uint, ulong, bool> AuthorizationReceived;

        public void Answer(ulong orderId, bool authorized) => AuthorizationReceived?.Invoke(480, orderId, authorized);
        public int Subscribers => AuthorizationReceived?.GetInvocationList().Length ?? 0;
    }

    public sealed class SteamStoreAdapterTests
    {
        private const string User = "player-1";
        private const ulong Order = 1790000000000000UL;
        private BillingConfig m_Config;
        private FakeSteamBridge m_Bridge;
        private FakeTransport m_Server;
        private MemoryBillingStorage m_Storage;
        private FakeApi m_Api;
        private BillingService m_Service;
        private SteamStoreAdapter m_Adapter;
        private Func<TimeSpan, CancellationToken, Task> m_Delay;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable));
            m_Config.WithBuild(BillingStores.Steam, "480");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Bridge = new FakeSteamBridge();
            m_Server = new FakeTransport { Next = Checkout(Order) };
            m_Storage = new MemoryBillingStorage();
            m_Api = new FakeApi();
            // The overlay answer never times out unless a test says so.
            m_Delay = (d, ct) => new TaskCompletionSource<bool>().Task;
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            m_Adapter?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private static HttpResponse Checkout(ulong orderId) => new HttpResponse(200, new JObject
        {
            ["status"] = "ok", ["order_id"] = orderId.ToString(), ["trans_id"] = "9001", ["currency"] = "EUR",
            ["amount"] = 179, ["environment"] = "production",
        }.ToString());

        private BillingService CreateService()
        {
            m_Adapter = new SteamStoreAdapter(m_Config, m_Bridge, null, m_Server, m_Storage) { Delay = m_Delay };
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = m_Adapter,
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
        public void Purchase_ChecksOutWithTheServer_ThenVerifiesTheAuthorizedOrder()
        {
            CreateService();
            Assert.AreEqual(BillingPlatform.Steam, m_Service.Platform);

            var purchase = m_Service.PurchaseAsync("gems_100");
            Assert.IsFalse(purchase.IsCompleted, "waits for the Steam overlay");
            var checkout = m_Server.Requests.Single();
            Assert.AreEqual(("POST", "https://billing.example.com/route/checkout/steam"), (checkout.Method, checkout.Url));
            var body = JObject.Parse(checkout.Body);
            Assert.AreEqual("480", (string)body["app_id"]);
            Assert.AreEqual("gems_100", (string)body["product_id"]);
            Assert.AreEqual(User, (string)body["user_id"]);
            Assert.AreEqual("76561197960287930", (string)body["steam_id"]);
            Assert.AreEqual("german", (string)body["language"]);
            Assert.IsEmpty(m_Api.VerifyRequests);

            m_Bridge.Answer(Order, true);
            var result = Sync.Run(purchase);

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            var verify = m_Api.VerifyRequests.Single();
            Assert.AreEqual(BillingStores.Steam, m_Api.VerifyStores.Single());
            Assert.AreEqual("480", verify.BundleId);
            Assert.AreEqual(Order.ToString(), verify.OrderId);
            Assert.AreEqual("gems_100", verify.ProductId);
            Assert.AreEqual(User, verify.UserId);
            Assert.AreEqual("[]", m_Storage.Load("purser.steam.authorized"), "an answered order is forgotten");
        }

        [Test]
        public void DeclinedInTheOverlay_IsCancelled_WithoutVerification()
        {
            CreateService();
            var purchase = m_Service.PurchaseAsync("no_ads");
            m_Bridge.Answer(Order, false);
            var result = Sync.Run(purchase);
            Assert.AreEqual(PurchaseStatus.Cancelled, result.Status);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void CheckoutError_FailsThePurchase()
        {
            m_Server.Next = new HttpResponse(403, "{\"status\":\"forbidden\",\"message\":\"this Steam account cannot make purchases (locked)\"}");
            CreateService();
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            StringAssert.Contains("locked", result.Message);

            m_Bridge.IsReady = false;
            m_Server.Requests.Clear();
            result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            Assert.IsEmpty(m_Server.Requests, "no checkout without Steam");
        }

        [Test]
        public void NoAnswerFromTheOverlay_TimesOut()
        {
            var timeout = new TaskCompletionSource<bool>();
            m_Delay = (d, ct) => timeout.Task;
            m_Api.DefaultVerify = new ApiResult<VerifyResponse>(200, new VerifyResponse
            {
                Status = VerifyStatus.Success, TransactionId = "9001", ProductId = "gems_100", Delivery = VerifyDelivery.Client,
            }, null);
            var succeeded = new List<PurchaseResult>();
            CreateService();
            m_Service.PurchaseSucceeded += succeeded.Add;
            var purchase = m_Service.PurchaseAsync("gems_100");
            Assert.IsFalse(purchase.IsCompleted);
            timeout.SetResult(true);
            Assert.AreEqual(PurchaseStatus.Failed, Sync.Run(purchase).Status);
            // A late answer for the forgotten order is still verified (the player did pay).
            m_Bridge.Answer(Order, true);
            Assert.AreEqual(1, m_Api.VerifyRequests.Count);
            Assert.AreEqual("", m_Api.VerifyRequests[0].ProductId, "the server knows the order's product");
            Assert.AreEqual("gems_100", succeeded.Single().ProductId, "reported as the product the server names, not a guess");
        }

        [Test]
        public void AuthorizedOrder_NotAnsweredByTheServer_IsVerifiedAgainAtTheNextLaunch()
        {
            for (var i = 0; i < 10; i++)
                m_Api.VerifyResults.Enqueue(FakeApi.TransportError());
            CreateService();
            var purchase = m_Service.PurchaseAsync("no_ads");
            m_Bridge.Answer(Order, true);
            Assert.AreEqual(PurchaseStatus.PendingVerification, Sync.Run(purchase).Status);
            StringAssert.Contains(Order.ToString(), m_Storage.Load("purser.steam.authorized"));
            m_Service.Dispose();
            m_Adapter.Dispose();
            Assert.AreEqual(0, m_Bridge.Subscribers, "the adapter lets go of the Steam callback");

            m_Api = new FakeApi();
            CreateService();
            var again = m_Api.VerifyRequests.Single();
            Assert.AreEqual(Order.ToString(), again.OrderId);
            Assert.AreEqual("no_ads", again.ProductId);
            Assert.AreEqual("[]", m_Storage.Load("purser.steam.authorized"));
        }

        [Test]
        public void CorruptStoredOrders_AreKept_NotOverwritten()
        {
            m_Storage.Save("purser.steam.authorized", "{not json");
            m_Adapter = new SteamStoreAdapter(m_Config, m_Bridge, null, m_Server, m_Storage, new BillingLog(BillingLogLevel.None));
            Sync.Run(m_Adapter.ConnectAsync(CancellationToken.None));
            Assert.AreEqual("{not json", m_Storage.Load("purser.steam.authorized.corrupt"));

            m_Storage.Save("purser.steam.authorized", "[null, {\"order_id\": \"5\", \"sku\": \"gems_100\", \"user_id\": \"u\", \"created_ticks\": " +
                                                    DateTime.UtcNow.Ticks + "}]");
            var raised = new List<StoreOrder>();
            m_Adapter = new SteamStoreAdapter(m_Config, m_Bridge, null, m_Server, m_Storage, new BillingLog(BillingLogLevel.None));
            m_Adapter.OrderPending += raised.Add;
            Assert.DoesNotThrow(() => Sync.Run(m_Adapter.ConnectAsync(CancellationToken.None)), "null entries are dropped");
            Assert.AreEqual("5", raised.Single().TransactionId);
        }

        [Test]
        public void WithoutSteam_InitializationFails()
        {
            m_Bridge.IsReady = false;
            m_Adapter = new SteamStoreAdapter(m_Config.serverBaseUrl, "480", m_Bridge, transport: m_Server, storage: m_Storage);
            Assert.Throws<BillingException>(() => Sync.Run(m_Adapter.ConnectAsync(CancellationToken.None)));
            Assert.Throws<ArgumentException>(() => new SteamStoreAdapter(m_Config.serverBaseUrl, "", m_Bridge));
        }
    }
}
