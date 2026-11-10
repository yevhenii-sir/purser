using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Billing server stand-in for the Xsolla routes: answers per path, synchronously.</summary>
    internal sealed class XsollaServer : IHttpTransport
    {
        public readonly List<(string Url, JObject Body)> Requests = new List<(string, JObject)>();
        public HttpResponse Checkout = new HttpResponse(200,
            "{\"status\":\"ok\",\"token\":\"tok-1\",\"order_id\":\"7001\",\"url\":\"https://secure.xsolla.com/paystation4/?token=tok-1\",\"sandbox\":false}");
        /// <summary>Status answers in order; the last one repeats.</summary>
        public readonly List<HttpResponse> Statuses = new List<HttpResponse>();
        private int m_Status;

        public Task<HttpResponse> SendAsync(string method, string url, string jsonBody, int timeoutSeconds, CancellationToken ct)
        {
            Requests.Add((url, JObject.Parse(jsonBody)));
            if (url.EndsWith("/checkout/xsolla"))
                return Task.FromResult(Checkout);
            var next = Statuses.Count == 0 ? Status("pending_or_canceled") : Statuses[Math.Min(m_Status, Statuses.Count - 1)];
            m_Status++;
            return Task.FromResult(next);
        }

        public int StatusCalls => Requests.Count(r => r.Url.EndsWith("/checkout/xsolla/status"));

        public static HttpResponse Status(string status, int http = 200, string delivery = "server") =>
            new HttpResponse(http, new JObject
            {
                ["status"] = status, ["message"] = "", ["order_id"] = "7001", ["transaction_id"] = "900001",
                ["product_id"] = "gems_100", ["delivery"] = delivery,
            }.ToString());
    }

    public sealed class XsollaStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private XsollaServer m_Server;
        private MemoryBillingStorage m_Storage;
        private FakeApi m_Api;
        private List<string> m_Opened;
        private DateTime m_Now;
        private BillingService m_Service;
        private XsollaStoreAdapter m_Adapter;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable));
            m_Config.webAppId = "com.test.game.web";
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Server = new XsollaServer();
            m_Storage = new MemoryBillingStorage();
            m_Api = new FakeApi();
            m_Opened = new List<string>();
            m_Now = new DateTime(2026, 11, 10, 12, 0, 0, DateTimeKind.Utc);
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService CreateService(XsollaOptions options = null)
        {
            options = options ?? new XsollaOptions();
            options.OpenUrl = url => m_Opened.Add(url);
            m_Adapter = new XsollaStoreAdapter(m_Config.serverBaseUrl, m_Config.AppIdFor(BillingPlatform.Web), 10, options,
                m_Server, m_Storage, delay: (d, ct) =>
                {
                    m_Now += d;
                    return Task.CompletedTask;
                }, utcNow: () => m_Now);
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
        public void Purchase_OpensPayStationAndPollsUntilGranted()
        {
            m_Server.Statuses.Add(XsollaServer.Status("pending_or_canceled"));
            m_Server.Statuses.Add(XsollaServer.Status("success"));
            CreateService(new XsollaOptions { ReturnUrl = "https://example.com/game", Country = "DE" });
            Assert.AreEqual(BillingPlatform.Web, m_Service.Platform);

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            Assert.AreEqual("900001", result.TransactionId);
            CollectionAssert.AreEqual(new[] { "https://secure.xsolla.com/paystation4/?token=tok-1" }, m_Opened);
            var checkout = m_Server.Requests.First();
            Assert.AreEqual("https://billing.example.com/route/checkout/xsolla", checkout.Url);
            Assert.AreEqual("com.test.game.web", (string)checkout.Body["app_id"]);
            Assert.AreEqual("gems_100", (string)checkout.Body["product_id"]);
            Assert.AreEqual(User, (string)checkout.Body["user_id"]);
            Assert.AreEqual("https://example.com/game", (string)checkout.Body["return_url"]);
            Assert.AreEqual("DE", (string)checkout.Body["country"]);
            Assert.AreEqual(2, m_Server.StatusCalls);
            var status = m_Server.Requests.Last().Body;
            Assert.AreEqual("7001", (string)status["order_id"]);
            Assert.AreEqual(User, (string)status["user_id"]);
            Assert.IsEmpty(m_Api.VerifyRequests, "there is no /verify/xsolla");
            Assert.AreEqual("[]", m_Storage.Load("purser.xsolla.pending"), "a granted order is forgotten");
        }

        [Test]
        public void UnpaidOrder_IsPendingPayment_AndCheckedAgainAtTheNextLaunch()
        {
            CreateService(new XsollaOptions { PaymentTimeout = TimeSpan.FromSeconds(30), PollInterval = TimeSpan.FromSeconds(10) });
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
            Assert.AreEqual(4, m_Server.StatusCalls, "polled until the payment timeout");
            m_Service.Dispose();

            // Paid after the app closed: the next launch finds it granted.
            m_Server = new XsollaServer();
            m_Server.Statuses.Add(XsollaServer.Status("success"));
            CreateService();
            Assert.AreEqual(1, m_Server.StatusCalls);
            Assert.AreEqual(0, m_Server.Requests.Count(r => r.Url.EndsWith("/checkout/xsolla")), "no new checkout");
            m_Service.Dispose();

            // Confirmed: not checked again.
            m_Server = new XsollaServer();
            CreateService();
            Assert.AreEqual(0, m_Server.StatusCalls);
        }

        [Test]
        public void CheckoutErrors_FailThePurchaseWithoutOpeningPayStation()
        {
            m_Server.Checkout = new HttpResponse(403, "{\"status\":\"forbidden\",\"message\":\"sandbox purchases are disabled for this app\"}");
            CreateService(new XsollaOptions { Sandbox = true });
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            StringAssert.Contains("sandbox purchases are disabled", result.Message);
            Assert.IsTrue((bool)m_Server.Requests.Single().Body["sandbox"]);
            Assert.IsEmpty(m_Opened);
        }

        [Test]
        public void RefundedOrder_IsRejectedAndForgotten()
        {
            m_Server.Statuses.Add(XsollaServer.Status("revoked"));
            CreateService();
            var result = Sync.Run(m_Service.PurchaseAsync("no_ads"));
            Assert.AreEqual(PurchaseStatus.Rejected, result.Status);
            Assert.AreEqual("[]", m_Storage.Load("purser.xsolla.pending"));
        }

        [Test]
        public void ClientDelivery_GrantsOnTheClient()
        {
            m_Server.Statuses.Add(XsollaServer.Status("success", delivery: "client"));
            CreateService();
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.IsTrue(result.GrantOnClient);
        }

        [Test]
        public void StoreName_IsXsolla()
        {
            CreateService();
            Assert.AreEqual("xsolla", ((IStoreNameProvider)m_Adapter).StoreName);
            Assert.IsTrue(Sync.Run(m_Service.RestoreAsync()) != null);
            Assert.IsEmpty(m_Api.RestoreRequests, "nothing to restore from Xsolla; entitlements come from the server");
        }
    }
}
