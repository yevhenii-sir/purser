using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser
{
    /// <summary>Settings of <see cref="StripeStoreAdapter"/>.</summary>
    public sealed class StripeCheckoutOptions
    {
        internal static readonly TimeSpan DefaultForgetAfter = TimeSpan.FromHours(25);

        /// <summary>https page Stripe returns to after payment (the game's page). May contain
        /// <c>{CHECKOUT_SESSION_ID}</c>: pass that id to <see cref="StripeStoreAdapter.ResumeSession"/>.</summary>
        public string SuccessUrl;

        /// <summary>https page Stripe returns to when the player goes back without paying.</summary>
        public string CancelUrl;

        /// <summary>Store product id (<see cref="BillingProductConfig.StoreId"/> for <c>stripe</c>) → Stripe Price id
        /// (the product id on the server). Products not listed use their store id as the Price id.</summary>
        public Dictionary<string, string> PriceIds = new Dictionary<string, string>();

        /// <summary>How often the server is asked whether the session is paid.</summary>
        public TimeSpan PollInterval = TimeSpan.FromSeconds(3);

        /// <summary>How long to wait for the payment before reporting <see cref="PurchaseStatus.PendingPayment"/>.
        /// The session is remembered and checked again at the next launch.</summary>
        public TimeSpan PaymentTimeout = TimeSpan.FromMinutes(10);

        /// <summary>Sessions older than this are forgotten (Checkout Sessions expire after 24 hours by default; a
        /// session paid anyway is granted by the webhook and shows in the entitlements).</summary>
        public TimeSpan ForgetAfter = DefaultForgetAfter;

        /// <summary>Opens the Checkout page (only https pages are opened). Default: <c>Application.OpenURL</c> (system browser on desktop, a new
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
    /// The app id is the <c>stripe</c> build of <see cref="BillingConfig.builds"/>; the product's id on the server is
    /// the Stripe Price id (<see cref="StripeCheckoutOptions.PriceIds"/>, or a <c>stripe</c> entry of
    /// <see cref="BillingProductConfig.storeIds"/>).
    /// </summary>
    public sealed class StripeStoreAdapter : ServerCheckoutStoreAdapter, IWebhookStoreAdapter
    {
        private const string PendingKey = "purser.stripe.sessions";

        private readonly string m_AppId;
        private readonly StripeCheckoutOptions m_Options;
        private readonly IStripeCheckoutApi m_Api;
        private readonly HashSet<string> m_Cancelled = new HashSet<string>();

        /// <inheritdoc/>
        public override BillingPlatform Platform => BillingPlatform.Web;
        /// <inheritdoc/>
        public override string Store => BillingStores.Stripe;

        /// <param name="config">Billing config: server URL and the app id of its <c>stripe</c> build.</param>
        /// <param name="options">Return URLs and Price ids.</param>
        /// <param name="api">Default: <see cref="BillingApiClient"/> for the config.</param>
        /// <param name="storage">Remembers unpaid sessions across launches (default: PlayerPrefs).</param>
        public StripeStoreAdapter(BillingConfig config, StripeCheckoutOptions options, IStripeCheckoutApi api = null,
            IBillingStorage storage = null, BillingLog log = null)
            : base(storage, log, PendingKey, options?.ForgetAfter ?? StripeCheckoutOptions.DefaultForgetAfter)
        {
            if (config == null)
                throw new ArgumentNullException(nameof(config));
            m_Options = options ?? throw new ArgumentNullException(nameof(options));
            if (string.IsNullOrEmpty(options.SuccessUrl) || string.IsNullOrEmpty(options.CancelUrl))
                throw new ArgumentException("StripeCheckoutOptions needs SuccessUrl and CancelUrl", nameof(options));
            m_AppId = config.AppIdFor(BillingStores.Stripe);
            m_Api = api ?? new BillingApiClient(config);
        }

        /// <summary>Checks a session the page was opened with (the <c>{CHECKOUT_SESSION_ID}</c> of the success URL).
        /// <paramref name="storeProductId"/> may be null: the server's answer names the product.
        /// <paramref name="userId"/>: the billing user it was bought for, when the session is not remembered.</summary>
        public void ResumeSession(string sessionId, string storeProductId = null, string userId = null)
        {
            if (string.IsNullOrEmpty(sessionId))
                return;
            var order = Remember(sessionId, storeProductId, string.IsNullOrEmpty(userId) ? CurrentUser : userId);
            RaisePending(ToOrder(order));
        }

        /// <summary>Stops waiting for the payment of a purchase (e.g. the player came back through the cancel page):
        /// it is reported as <see cref="PurchaseStatus.PendingPayment"/> and checked again at the next launch.</summary>
        public void CancelCheckout(string storeProductId)
        {
            if (!string.IsNullOrEmpty(storeProductId))
                m_Cancelled.Add(storeProductId);
        }

        protected override async Task StartCheckoutAsync(string sku, string userId)
        {
            m_Cancelled.Remove(sku);
            var request = new StripeCheckoutRequest
            {
                AppId = m_AppId,
                ProductId = PriceId(sku),
                UserId = userId,
                SuccessUrl = m_Options.SuccessUrl,
                CancelUrl = m_Options.CancelUrl,
            };
            var res = await m_Api.CreateStripeCheckoutAsync(request, Lifetime);
            var checkout = res?.Value;
            if (res == null || !res.IsHttpSuccess || string.IsNullOrEmpty(checkout?.Url) || string.IsNullOrEmpty(checkout.SessionId))
            {
                Fail(sku, "checkout: " + (checkout?.Message ?? res?.Error ?? "no answer"));
                return;
            }
            if (!IsSafePage(sku, checkout.Url))
                return;
            var order = Remember(checkout.SessionId, sku, userId);
            (m_Options.OpenUrl ?? Application.OpenURL)(checkout.Url);
            RaisePending(ToOrder(order, checkout));
        }

        /// <summary>Polls <c>/verify/stripe</c> until the session is granted or rejected, or the payment timeout passes
        /// (then <c>pending_or_canceled</c>). Sends the attribution and paywall of <paramref name="request"/>.</summary>
        public Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, VerifyRequest request, CancellationToken ct)
        {
            var user = BuyerOf(order);
            if (user == null)
                return Task.FromResult(Answer(VerifyStatus.PendingOrCanceled, "the session belongs to another user"));
            var session = new StripeSessionRequest
            {
                AppId = m_AppId,
                UserId = user,
                SessionId = order.TransactionId,
                AppInstanceId = request?.AppInstanceId,
                Attribution = request?.Attribution,
                Placement = request?.Placement,
                VariantId = request?.VariantId,
                TestId = request?.TestId,
                PaywallId = request?.PaywallId,
            };
            return PollAsync(token => m_Api.VerifyStripeSessionAsync(session, token), m_Options.PollInterval, m_Options.PaymentTimeout,
                () => !string.IsNullOrEmpty(order.StoreProductId) && m_Cancelled.Remove(order.StoreProductId), ct);
        }

        /// <summary>Session ids waiting for an answer (tests, diagnostics).</summary>
        internal IReadOnlyList<string> PendingSessionIds => PendingOrderIds;

        private string PriceId(string storeProductId) =>
            m_Options.PriceIds != null && m_Options.PriceIds.TryGetValue(storeProductId, out var price) && !string.IsNullOrEmpty(price)
                ? price
                : storeProductId;
    }
}
