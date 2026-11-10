using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;
using UnityEngine;

namespace Purser
{
    /// <summary>Settings of <see cref="StripeStoreAdapter"/>.</summary>
    public sealed class StripeCheckoutOptions
    {
        /// <summary>https page Stripe returns to after payment (the game's page). May contain
        /// <c>{CHECKOUT_SESSION_ID}</c>: pass that id to <see cref="StripeStoreAdapter.ResumeSession"/>.</summary>
        public string SuccessUrl;

        /// <summary>https page Stripe returns to when the player goes back without paying.</summary>
        public string CancelUrl;

        /// <summary>Store product id (<see cref="BillingProductConfig.StoreId"/> of the web build) → Stripe Price id
        /// (the product id on the server). Products not listed use their store id as the Price id.</summary>
        public Dictionary<string, string> PriceIds = new Dictionary<string, string>();

        /// <summary>How often the server is asked whether the session is paid.</summary>
        public TimeSpan PollInterval = TimeSpan.FromSeconds(3);

        /// <summary>How long to wait for the payment before reporting <see cref="PurchaseStatus.PendingPayment"/>.
        /// The session is remembered and checked again at the next launch.</summary>
        public TimeSpan PaymentTimeout = TimeSpan.FromMinutes(10);

        /// <summary>Sessions older than this are forgotten (Checkout Sessions expire after 24 hours by default; a
        /// session paid anyway is granted by the webhook and shows in the entitlements).</summary>
        public TimeSpan ForgetAfter = TimeSpan.FromHours(25);

        /// <summary>Opens the Checkout page. Default: <c>Application.OpenURL</c> (system browser on desktop, a new
        /// tab on WebGL). On WebGL a page opened after an await may be blocked as a popup: redirect the tab from a
        /// jslib instead (the session is checked again when the game loads).</summary>
        public Action<string> OpenUrl;
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for web builds selling through Stripe Checkout (WebGL, or any platform that can open
    /// a browser). Purchase: the billing server creates a Checkout Session (<c>/checkout/stripe</c>), the adapter
    /// opens its page, and the session is polled (<c>/verify/stripe</c>, which records it as soon as it is paid,
    /// without waiting for Stripe's webhook) until it is granted. Unpaid sessions are remembered and checked again at
    /// the next launch. Stripe has no client SDK dependency here.
    /// <code>new BillingDependencies { Store = new StripeStoreAdapter(config, new StripeCheckoutOptions { SuccessUrl = "https://example.com/game", CancelUrl = "https://example.com/game" }) }</code>
    /// The product's id on the server is the Stripe Price id (<see cref="StripeCheckoutOptions.PriceIds"/>).
    /// </summary>
    public sealed class StripeStoreAdapter : IStoreAdapter, IStoreNameProvider, IWebhookStoreAdapter
    {
        public const string Store = "stripe";
        private const string PendingKey = "purser.stripe.sessions";

        private sealed class PendingSession
        {
            [JsonProperty("session_id")] public string SessionId;
            [JsonProperty("product")] public string StoreProductId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("created_ticks")] public long CreatedTicks;
        }

        private readonly string m_AppId;
        private readonly StripeCheckoutOptions m_Options;
        private readonly IStripeCheckoutApi m_Api;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly HashSet<string> m_Cancelled = new HashSet<string>();
        private List<PendingSession> m_Pending;
        private string m_User;

        // Test hooks.
        internal Func<TimeSpan, CancellationToken, Task> Delay = (d, ct) => Task.Delay(d, ct);
        internal Func<DateTime> UtcNow = () => DateTime.UtcNow;

        public BillingPlatform Platform => BillingPlatform.Web;
        public string StoreName => Store;

        public event Action<StoreOrder> OrderPending;
        /// <summary>Not used: an unpaid session is reported by <see cref="CheckOrderAsync"/>.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        public event Action<StorePurchaseFailure> PurchaseFailed;
        public event Action<StoreOrder, bool, string> OrderConfirmed;

        /// <param name="config">Billing config: server URL and the web build's app id (<see cref="BillingConfig.webAppId"/>).</param>
        /// <param name="options">Return URLs and Price ids.</param>
        /// <param name="api">Default: <see cref="BillingApiClient"/> for the config.</param>
        /// <param name="storage">Remembers unpaid sessions across launches (default: PlayerPrefs).</param>
        public StripeStoreAdapter(BillingConfig config, StripeCheckoutOptions options, IStripeCheckoutApi api = null,
            IBillingStorage storage = null, BillingLog log = null)
        {
            if (config == null)
                throw new ArgumentNullException(nameof(config));
            m_Options = options ?? throw new ArgumentNullException(nameof(options));
            if (string.IsNullOrEmpty(options.SuccessUrl) || string.IsNullOrEmpty(options.CancelUrl))
                throw new ArgumentException("StripeCheckoutOptions needs SuccessUrl and CancelUrl", nameof(options));
            m_AppId = config.AppIdFor(BillingPlatform.Web);
            m_Api = api ?? new BillingApiClient(config);
            m_Storage = storage ?? new PlayerPrefsBillingStorage();
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
        }

        /// <summary>Re-raises sessions of earlier launches that were not confirmed (paid after the game closed or the
        /// tab was redirected to Stripe, or not yet).</summary>
        public Task ConnectAsync(CancellationToken ct)
        {
            var cutoff = UtcNow() - m_Options.ForgetAfter;
            if (Pending.RemoveAll(s => new DateTime(s.CreatedTicks, DateTimeKind.Utc) < cutoff) > 0)
                SavePending();
            ResumePendingOrders();
            return Task.CompletedTask;
        }

        /// <summary>Checks the remembered unpaid sessions again; results arrive as purchase events of the billing service.</summary>
        public void ResumePendingOrders()
        {
            foreach (var s in Pending.ToList())
                OrderPending?.Invoke(ToOrder(s));
        }

        /// <summary>Checks a session the page was opened with (the <c>{CHECKOUT_SESSION_ID}</c> of the success URL).
        /// <paramref name="userId"/>: the billing user it was bought for, when the session is not remembered.</summary>
        public void ResumeSession(string sessionId, string storeProductId, string userId = null)
        {
            if (string.IsNullOrEmpty(sessionId))
                return;
            var s = Pending.FirstOrDefault(x => x.SessionId == sessionId);
            if (s == null)
            {
                s = new PendingSession { SessionId = sessionId, StoreProductId = storeProductId, UserId = string.IsNullOrEmpty(userId) ? m_User : userId, CreatedTicks = UtcNow().Ticks };
                Pending.Add(s);
                SavePending();
            }
            OrderPending?.Invoke(ToOrder(s));
        }

        /// <summary>Stops waiting for the payment of a purchase (e.g. the player came back through the cancel page):
        /// it is reported as <see cref="PurchaseStatus.PendingPayment"/> and checked again at the next launch.</summary>
        public void CancelCheckout(string storeProductId)
        {
            if (!string.IsNullOrEmpty(storeProductId))
                m_Cancelled.Add(storeProductId);
        }

        /// <summary>Products from the config: prices are not loaded from Stripe (show them from your paywall config).</summary>
        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            IReadOnlyList<StoreProduct> list = products
                .Select(p => new StoreProduct(p.id, p.StoreId(Platform), p.type, "", 0m, null, p.id, "", true))
                .ToList();
            return Task.FromResult(list);
        }

        /// <summary>Stripe has no account binding field: the server puts the billing user id into the session.</summary>
        public void SetAccountBinding(string googleAccountId, Guid? appleAccountToken) { }

        public void SetBillingUser(string userId) => m_User = userId;

        public void Purchase(string storeProductId) => _ = PurchaseCoreAsync(storeProductId);

        private async Task PurchaseCoreAsync(string storeProductId)
        {
            try
            {
                m_Cancelled.Remove(storeProductId);
                var user = m_User;
                if (string.IsNullOrEmpty(user))
                {
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Stripe: the billing user id is not set", false));
                    return;
                }
                var request = new StripeCheckoutRequest
                {
                    AppId = m_AppId,
                    ProductId = PriceId(storeProductId),
                    UserId = user,
                    SuccessUrl = m_Options.SuccessUrl,
                    CancelUrl = m_Options.CancelUrl,
                };
                var res = await m_Api.CreateStripeCheckoutAsync(request, CancellationToken.None);
                var checkout = res?.Value;
                if (res == null || !res.IsHttpSuccess || string.IsNullOrEmpty(checkout?.Url) || string.IsNullOrEmpty(checkout.SessionId))
                {
                    var reason = checkout?.Message ?? res?.Error ?? "no answer";
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Stripe checkout: " + reason, false));
                    return;
                }
                var pending = new PendingSession { SessionId = checkout.SessionId, StoreProductId = storeProductId, UserId = user, CreatedTicks = UtcNow().Ticks };
                Pending.Add(pending);
                SavePending();
                (m_Options.OpenUrl ?? Application.OpenURL)(checkout.Url);
                OrderPending?.Invoke(ToOrder(pending, checkout));
            }
            catch (Exception e)
            {
                m_Log.Error("Stripe checkout failed: " + e);
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Stripe checkout: " + e.Message, false));
            }
        }

        /// <summary>Polls <c>/verify/stripe</c> until the session is granted or rejected, or the payment timeout passes
        /// (then <c>pending_or_canceled</c>).</summary>
        public async Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, CancellationToken ct)
        {
            var user = string.IsNullOrEmpty(order.StoreUserId) ? m_User : order.StoreUserId;
            if (string.IsNullOrEmpty(user) || (!string.IsNullOrEmpty(m_User) && user != m_User))
                // Paid by another player of this device: granted to that player by the server; not reported here.
                return Answer(VerifyStatus.PendingOrCanceled, "the session belongs to another user");
            var request = new StripeSessionRequest { AppId = m_AppId, UserId = user, SessionId = order.TransactionId };
            var deadline = UtcNow() + m_Options.PaymentTimeout;
            while (true)
            {
                if (order.StoreProductId != null && m_Cancelled.Remove(order.StoreProductId))
                    return Answer(VerifyStatus.PendingOrCanceled, "checkout cancelled");
                var res = await m_Api.VerifyStripeSessionAsync(request, ct);
                var status = res?.Value?.Status;
                var unpaid = res == null || status == VerifyStatus.PendingOrCanceled || res.IsTransportError || res.HttpStatus >= 500;
                if (!unpaid || UtcNow() >= deadline)
                    return res;
                await Delay(m_Options.PollInterval, ct);
            }
        }

        /// <summary>The server answered for the session: it is no longer checked at launch.</summary>
        public void Confirm(StoreOrder order)
        {
            if (order?.TransactionId == null)
                return;
            if (Pending.RemoveAll(s => s.SessionId == order.TransactionId) > 0)
                SavePending();
            OrderConfirmed?.Invoke(order, true, null);
        }

        /// <summary>Owned products come from the server's entitlements: nothing to restore from the store.</summary>
        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<StoreOrder>>(new List<StoreOrder>());

        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        public void Dispose() { }

        // ---- helpers ----

        /// <summary>Session ids waiting for an answer (tests, diagnostics).</summary>
        internal IReadOnlyList<string> PendingSessionIds => Pending.Select(s => s.SessionId).ToList();

        private List<PendingSession> Pending => m_Pending ?? (m_Pending = LoadPending());

        private string PriceId(string storeProductId) =>
            m_Options.PriceIds != null && m_Options.PriceIds.TryGetValue(storeProductId, out var price) && !string.IsNullOrEmpty(price)
                ? price
                : storeProductId;

        private StoreOrder ToOrder(PendingSession s, object native = null) =>
            new StoreOrder(Platform, s.StoreProductId, s.SessionId, null, s.SessionId, null, native, s.UserId);

        private static ApiResult<VerifyResponse> Answer(string status, string message) =>
            new ApiResult<VerifyResponse>(200, new VerifyResponse { Status = status, Message = message }, null);

        private List<PendingSession> LoadPending()
        {
            try
            {
                var json = m_Storage.Load(PendingKey);
                var list = string.IsNullOrEmpty(json)
                    ? new List<PendingSession>()
                    : JsonConvert.DeserializeObject<List<PendingSession>>(json) ?? new List<PendingSession>();
                list.RemoveAll(s => string.IsNullOrEmpty(s?.SessionId));
                return list;
            }
            catch (Exception)
            {
                return new List<PendingSession>();
            }
        }

        private void SavePending()
        {
            try
            {
                m_Storage.Save(PendingKey, JsonConvert.SerializeObject(Pending));
            }
            catch (Exception e)
            {
                m_Log.Warning("Could not remember Stripe sessions: " + e.Message);
            }
        }
    }
}
