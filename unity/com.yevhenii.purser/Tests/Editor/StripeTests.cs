using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Billing server Stripe routes, scripted per test; answers synchronously.</summary>
    internal sealed class FakeStripeApi : IStripeCheckoutApi
    {
        public readonly List<StripeCheckoutRequest> Checkouts = new List<StripeCheckoutRequest>();
        public readonly List<StripeSessionRequest> SessionChecks = new List<StripeSessionRequest>();
        public ApiResult<StripeCheckoutResponse> CheckoutResult = new ApiResult<StripeCheckoutResponse>(200,
            new StripeCheckoutResponse { Status = "ok", SessionId = "cs_test_1", Url = "https://checkout.stripe.com/c/pay/cs_test_1" }, null);
        /// <summary>Answers of /verify/stripe in order; the last one repeats.</summary>
        public readonly List<string> SessionStatuses = new List<string> { VerifyStatus.Success };

        public Task<ApiResult<StripeCheckoutResponse>> CreateStripeCheckoutAsync(StripeCheckoutRequest request, CancellationToken ct = default)
        {
            Checkouts.Add(request);
            return Task.FromResult(CheckoutResult);
        }

        public Task<ApiResult<VerifyResponse>> VerifyStripeSessionAsync(StripeSessionRequest request, CancellationToken ct = default)
        {
            SessionChecks.Add(request);
            var status = SessionStatuses[Math.Min(SessionChecks.Count - 1, SessionStatuses.Count - 1)];
            return Task.FromResult(new ApiResult<VerifyResponse>(200,
                new VerifyResponse { Status = status, TransactionId = "pi_1", ProductId = "price_gems" }, null));
        }
    }

    public sealed class StripeStoreAdapterTests
    {
        private const string User = "player-1";
        private BillingConfig m_Config;
        private FakeStripeApi m_Stripe;
        private List<string> m_Opened;
        private MemoryBillingStorage m_Storage;
        private FakeApi m_Api;
        private DateTime m_Now;
        private StripeStoreAdapter m_Adapter;
        private BillingService m_Service;
        private readonly List<PurchaseResult> m_Events = new List<PurchaseResult>();

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("vip", BillingProductType.Subscription));
            m_Config.webAppId = "com.test.game.web";
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Stripe = new FakeStripeApi();
            m_Opened = new List<string>();
            m_Storage = new MemoryBillingStorage();
            m_Api = new FakeApi();
            m_Now = new DateTime(2026, 11, 10, 12, 0, 0, DateTimeKind.Utc);
            m_Events.Clear();
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private StripeCheckoutOptions Options() => new StripeCheckoutOptions
        {
            SuccessUrl = "https://game.example/play?session={CHECKOUT_SESSION_ID}",
            CancelUrl = "https://game.example/play",
            PriceIds = { ["gems_100"] = "price_gems" },
            PollInterval = TimeSpan.FromSeconds(1),
            PaymentTimeout = TimeSpan.FromSeconds(2),
            OpenUrl = url => m_Opened.Add(url),
        };

        private BillingService CreateService()
        {
            m_Adapter = new StripeStoreAdapter(m_Config, Options(), m_Stripe, m_Storage)
            {
                // Waiting moves the clock: the payment timeout passes without real time.
                Delay = (d, ct) =>
                {
                    m_Now += d;
                    return Task.CompletedTask;
                },
                UtcNow = () => m_Now,
            };
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = m_Adapter,
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider(User),
                Storage = new MemoryBillingStorage(),
                Delay = (d, ct) => Task.CompletedTask,
                IsEditor = false,
            });
            m_Service.PurchaseSucceeded += r => m_Events.Add(r);
            m_Service.PurchasePending += r => m_Events.Add(r);
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void Purchase_OpensCheckout_AndPollsTheSessionUntilGranted()
        {
            m_Stripe.SessionStatuses.Clear();
            m_Stripe.SessionStatuses.AddRange(new[] { VerifyStatus.PendingOrCanceled, VerifyStatus.PendingOrCanceled, VerifyStatus.Success });
            CreateService();
            Assert.AreEqual(BillingPlatform.Web, m_Service.Platform);

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Success, result.Status, result.Message);
            var checkout = m_Stripe.Checkouts.Single();
            Assert.AreEqual(("com.test.game.web", "price_gems", User), (checkout.AppId, checkout.ProductId, checkout.UserId));
            Assert.AreEqual("https://game.example/play?session={CHECKOUT_SESSION_ID}", checkout.SuccessUrl);
            CollectionAssert.AreEqual(new[] { "https://checkout.stripe.com/c/pay/cs_test_1" }, m_Opened);
            Assert.AreEqual(3, m_Stripe.SessionChecks.Count, "polled until paid");
            Assert.AreEqual(("com.test.game.web", User, "cs_test_1"),
                (m_Stripe.SessionChecks[0].AppId, m_Stripe.SessionChecks[0].UserId, m_Stripe.SessionChecks[0].SessionId));
            Assert.IsEmpty(m_Api.VerifyRequests, "a webhook store has no receipt for /verify");
            Assert.IsEmpty(m_Adapter.PendingSessionIds, "answered sessions are forgotten");
        }

        [Test]
        public void UnpaidCheckout_IsPendingAfterTheTimeout_AndCheckedAgainAtTheNextLaunch()
        {
            m_Stripe.SessionStatuses[0] = VerifyStatus.PendingOrCanceled;
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
            Assert.AreEqual(3, m_Stripe.SessionChecks.Count, "checks until PaymentTimeout");
            CollectionAssert.AreEqual(new[] { "cs_test_1" }, m_Adapter.PendingSessionIds);
            m_Service.Dispose();

            // Paid after all (e.g. the tab was redirected to Stripe and the game loaded again).
            m_Stripe = new FakeStripeApi();
            CreateService();
            Assert.AreEqual(("cs_test_1", User), (m_Stripe.SessionChecks.Single().SessionId, m_Stripe.SessionChecks.Single().UserId));
            Assert.AreEqual(PurchaseStatus.Success, m_Events.Last().Status);
            Assert.IsEmpty(m_Adapter.PendingSessionIds);
        }

        [Test]
        public void CancelCheckout_StopsWaiting()
        {
            m_Stripe.SessionStatuses[0] = VerifyStatus.PendingOrCanceled;
            CreateService();
            m_Adapter.CancelCheckout("gems_100");
            // Cancelled before the purchase starts is ignored; cancelling while it waits is what counts.
            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));
            Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
            var checks = m_Stripe.SessionChecks.Count;
            m_Adapter.CancelCheckout("gems_100");
            m_Adapter.ResumePendingOrders();
            Assert.AreEqual(checks, m_Stripe.SessionChecks.Count, "no session check after a cancel");
        }

        [Test]
        public void CheckoutErrors_AreReportedWithoutOpeningAPage()
        {
            m_Stripe.CheckoutResult = new ApiResult<StripeCheckoutResponse>(400,
                new StripeCheckoutResponse { Status = "error", Message = "unknown product price_gems" }, "HTTP 400");
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100"));

            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            StringAssert.Contains("unknown product price_gems", result.Message);
            Assert.IsEmpty(m_Opened);
            Assert.IsEmpty(m_Stripe.SessionChecks);
        }

        [Test]
        public void RejectedSession_IsReportedAndForgotten()
        {
            m_Stripe.SessionStatuses[0] = VerifyStatus.Forbidden;
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("vip"));

            Assert.AreEqual(PurchaseStatus.Rejected, result.Status);
            Assert.AreEqual("vip", m_Stripe.Checkouts.Single().ProductId, "unmapped products use their store id as the Price id");
            Assert.IsEmpty(m_Adapter.PendingSessionIds);
        }

        [Test]
        public void ResumeSession_ChecksASessionFromTheSuccessUrl()
        {
            CreateService();
            m_Adapter.ResumeSession("cs_test_9", "gems_100", User);
            Assert.AreEqual(("cs_test_9", User), (m_Stripe.SessionChecks.Single().SessionId, m_Stripe.SessionChecks.Single().UserId));
            Assert.AreEqual(PurchaseStatus.Success, m_Events.Single().Status);
        }

        [Test]
        public void Options_NeedReturnUrls()
        {
            Assert.Throws<ArgumentException>(() => new StripeStoreAdapter(m_Config, new StripeCheckoutOptions(), m_Stripe, m_Storage));
        }

        [Test]
        public void ApiClient_PostsTheStripeRoutes()
        {
            var transport = new FakeTransport { Next = new HttpResponse(200, "{\"status\":\"ok\",\"session_id\":\"cs_1\",\"url\":\"https://checkout.stripe.com/x\"}") };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);

            var created = Sync.Run(api.CreateStripeCheckoutAsync(new StripeCheckoutRequest
            {
                AppId = "a", ProductId = "price_1", UserId = "u", SuccessUrl = "https://s", CancelUrl = "https://c",
            }));
            Assert.AreEqual(("cs_1", "https://checkout.stripe.com/x"), (created.Value.SessionId, created.Value.Url));
            Assert.AreEqual("https://b.example.com/route/checkout/stripe", transport.Requests[0].Url);
            var body = JObject.Parse(transport.Requests[0].Body);
            Assert.AreEqual(("a", "price_1", "u", "https://s", "https://c"),
                ((string)body["app_id"], (string)body["product_id"], (string)body["user_id"], (string)body["success_url"], (string)body["cancel_url"]));

            Sync.Run(api.VerifyStripeSessionAsync(new StripeSessionRequest { AppId = "a", UserId = "u", SessionId = "cs_1" }));
            Assert.AreEqual("https://b.example.com/route/verify/stripe", transport.Requests[1].Url);
            Assert.AreEqual("cs_1", (string)JObject.Parse(transport.Requests[1].Body)["session_id"]);
        }
    }
}
