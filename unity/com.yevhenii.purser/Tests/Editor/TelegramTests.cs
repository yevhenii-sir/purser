using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Billing server stand-in for the Telegram routes: answers per path, synchronously.</summary>
    internal sealed class TelegramServer : IHttpTransport
    {
        public readonly List<(string Url, JObject Body)> Requests = new List<(string, JObject)>();
        public HttpResponse Checkout = new HttpResponse(200,
            "{\"status\":\"ok\",\"url\":\"https://t.me/$inv-1\",\"order_id\":\"tg0123456789abcdef0123456789abcd\"}");
        /// <summary>Status answers in order; the last one repeats.</summary>
        public readonly List<HttpResponse> Statuses = new List<HttpResponse>();
        private int m_Status;

        public Task<HttpResponse> SendAsync(string method, string url, string jsonBody, IReadOnlyDictionary<string, string> headers,
            int timeoutSeconds, CancellationToken ct)
        {
            Requests.Add((url, JObject.Parse(jsonBody)));
            if (url.EndsWith("/checkout/telegram"))
                return Task.FromResult(Checkout);
            var next = Statuses.Count == 0 ? Status("pending_or_canceled") : Statuses[Math.Min(m_Status, Statuses.Count - 1)];
            m_Status++;
            return Task.FromResult(next);
        }

        public int StatusCalls => Requests.Count(r => r.Url.EndsWith("/checkout/telegram/status"));

        public static HttpResponse Status(string status) =>
            new HttpResponse(200, new JObject
            {
                ["status"] = status, ["message"] = "", ["order_id"] = "tg0123456789abcdef0123456789abcd", ["transaction_id"] = "charge-1",
                ["product_id"] = "gems_100", ["delivery"] = "server",
            }.ToString());
    }

    /// <summary>Records opened invoices; <see cref="Close"/> answers like Telegram.</summary>
    internal sealed class FakeTelegramWebApp : ITelegramWebApp
    {
        public readonly List<string> Opened = new List<string>();
        /// <summary>Answered right away when set.</summary>
        public string AnswerWith;
        private Action<string> m_Closed;

        public void OpenInvoice(string url, Action<string> closed)
        {
            Opened.Add(url);
            m_Closed = closed;
            if (AnswerWith != null)
                closed(AnswerWith);
        }

        public void Close(string status) => m_Closed?.Invoke(status);
    }

    public sealed class TelegramStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private TelegramServer m_Server;
        private FakeTelegramWebApp m_WebApp;
        private DateTime m_Now;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("vip", BillingProductType.Subscription));
            m_Config.WithBuild(BillingStores.Telegram, "com.test.game.telegram");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.restoreOnFirstLaunch = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Server = new TelegramServer();
            m_WebApp = new FakeTelegramWebApp();
            m_Now = new DateTime(2026, 11, 25, 12, 0, 0, DateTimeKind.Utc);
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService CreateService()
        {
            var adapter = new TelegramStoreAdapter(m_Config, m_WebApp, new TelegramOptions(), m_Server, new MemoryBillingStorage())
            {
                Delay = (d, ct) =>
                {
                    m_Now += d;
                    return Task.CompletedTask;
                },
                UtcNow = () => m_Now,
            };
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = adapter,
                Api = new FakeApi(),
                UserIdProvider = new StaticUserIdProvider(User),
                Storage = new MemoryBillingStorage(),
                Delay = (d, ct) => Task.CompletedTask,
                IsEditor = false,
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void Purchase_OpensTheInvoiceAndPollsUntilGranted()
        {
            m_Server.Statuses.Add(TelegramServer.Status("pending_or_canceled"));
            m_Server.Statuses.Add(TelegramServer.Status("success"));
            m_WebApp.AnswerWith = TelegramInvoiceStatus.Paid;
            CreateService();
            Assert.AreEqual(BillingPlatform.Web, m_Service.Platform);

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.ToString());
            Assert.AreEqual("charge-1", result.TransactionId);
            CollectionAssert.AreEqual(new[] { "https://t.me/$inv-1" }, m_WebApp.Opened);
            var checkout = m_Server.Requests.First();
            Assert.AreEqual("https://billing.example.com/route/checkout/telegram", checkout.Url);
            Assert.AreEqual(("com.test.game.telegram", "gems_100", User),
                ((string)checkout.Body["app_id"], (string)checkout.Body["product_id"], (string)checkout.Body["user_id"]));
            var status = m_Server.Requests.Last();
            Assert.AreEqual("https://billing.example.com/route/checkout/telegram/status", status.Url);
            Assert.AreEqual("tg0123456789abcdef0123456789abcd", (string)status.Body["order_id"]);
        }

        [Test]
        public void AnInvoiceClosedWithoutPayingStopsTheChecks()
        {
            m_WebApp.AnswerWith = TelegramInvoiceStatus.Cancelled;
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreNotEqual(PurchaseStatus.Success, result.Status);
            Assert.LessOrEqual(m_Server.StatusCalls, 1, "no polling after the player closed the invoice");
        }

        [Test]
        public void ACheckoutTheServerRefusesFails()
        {
            m_Server.Checkout = new HttpResponse(400, "{\"status\":\"error\",\"message\":\"gems_100: the product's price must be in XTR\"}");
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreNotEqual(PurchaseStatus.Success, result.Status);
            StringAssert.Contains("XTR", result.Message);
            Assert.IsEmpty(m_WebApp.Opened);
        }
    }
}
