using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;
using UnityEngine;

namespace Purser
{
    /// <summary>Settings of <see cref="XsollaStoreAdapter"/>.</summary>
    public sealed class XsollaOptions
    {
        /// <summary>Pay with the Xsolla sandbox (test cards). The build must allow sandbox purchases on the server.</summary>
        public bool Sandbox;

        /// <summary>Where Pay Station sends the player after paying (optional): the game's page (https), or in a mobile
        /// game an app link of its own, e.g. <c>mygame://purchase</c> (register the scheme as the app's deep link): the
        /// player comes back right away and the order is checked at once.</summary>
        public string ReturnUrl;

        /// <summary>ISO 3166-1 alpha-2 country of the player; Xsolla picks the currency from it and lets the player
        /// change it. Empty: the server sends the player's IP address when it knows it.</summary>
        public string Country;

        /// <summary>Two-letter language of the payment UI, e.g. <c>en</c> (optional).</summary>
        public string Language;

        /// <summary>How often the order status is asked while the player pays.</summary>
        public TimeSpan PollInterval = TimeSpan.FromSeconds(3);

        /// <summary>How long to wait for the payment before reporting <see cref="PurchaseStatus.PendingPayment"/>.
        /// The order is remembered and checked again at the next launch.</summary>
        public TimeSpan PaymentTimeout = TimeSpan.FromMinutes(10);

        /// <summary>Orders older than this are forgotten (never paid).</summary>
        public TimeSpan ForgetAfter = TimeSpan.FromDays(2);

        /// <summary>Opens the Pay Station page (only https pages are opened). Default: <see cref="PurserBrowser.Open"/> (Chrome
        /// Custom Tabs on Android, the browser on iOS and desktop, a new tab on WebGL). On WebGL a page opened after an await may be blocked as a popup: open it from a jslib
        /// (same tab, or a window opened in the click handler) instead.</summary>
        public Action<string> OpenUrl;
    }

    /// <summary>Answer of <c>POST /checkout/xsolla</c>.</summary>
    public sealed class XsollaCheckoutResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        [JsonProperty("token")] public string Token;
        [JsonProperty("order_id")] public string OrderId;
        /// <summary>Pay Station page to open.</summary>
        [JsonProperty("url")] public string Url;
        [JsonProperty("sandbox")] public bool Sandbox;
        [JsonProperty("product_id")] public string ProductId;
    }

    /// <summary>An item moved from the inventory of the old client-side Xsolla integration.</summary>
    public sealed class XsollaClaimedItem
    {
        [JsonProperty("product_id")] public string ProductId;
        /// <summary>The ledger status of the moved item (<c>granted</c>, <c>completed</c>, ...).</summary>
        [JsonProperty("purchase_status")] public string PurchaseStatus;
    }

    /// <summary>An inventory item that was not moved, and why.</summary>
    public sealed class XsollaSkippedItem
    {
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("reason")] public string Reason;
    }

    /// <summary>Answer of <c>POST /claim/xsolla</c>.</summary>
    public sealed class XsollaClaimResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        [JsonProperty("claimed")] public List<XsollaClaimedItem> Claimed = new List<XsollaClaimedItem>();
        [JsonProperty("skipped")] public List<XsollaSkippedItem> Skipped = new List<XsollaSkippedItem>();
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for Xsolla Pay Station (web, desktop, and mobile games next to the app store through
    /// <see cref="CompositeStoreAdapter"/>). Purchase: the billing server creates a payment
    /// token (<c>/checkout/xsolla</c>), the adapter opens Pay Station, and the order is polled
    /// (<c>/checkout/xsolla/status</c>) until Xsolla's webhook reached the server and the reward was granted.
    /// Unpaid orders are remembered and checked again at the next launch. Xsolla has no client SDK dependency here.
    /// <code>new BillingDependencies { Store = new XsollaStoreAdapter(config, new XsollaOptions { ReturnUrl = "https://example.com/game" }) }</code>
    /// The app id is the <c>xsolla</c> build of <see cref="BillingConfig.builds"/>; the product's store id is the Xsolla
    /// item SKU. Subscriptions are not sold through this adapter.
    /// </summary>
    public sealed class XsollaStoreAdapter : ServerCheckoutStoreAdapter, IWebhookStoreAdapter
    {
        private const string PendingKey = "purser.xsolla.pending";

        private sealed class CheckoutRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("product_id")] public string ProductId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("sandbox")] public bool Sandbox;
            [JsonProperty("return_url", NullValueHandling = NullValueHandling.Ignore)] public string ReturnUrl;
            [JsonProperty("country", NullValueHandling = NullValueHandling.Ignore)] public string Country;
            [JsonProperty("language", NullValueHandling = NullValueHandling.Ignore)] public string Language;
            [JsonProperty("attribution", NullValueHandling = NullValueHandling.Ignore)] public IReadOnlyDictionary<string, string> Attribution;
        }

        private sealed class ClaimRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("xsolla_token")] public string XsollaToken;
        }

        private sealed class StatusRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("order_id")] public string OrderId;
        }

        private readonly string m_AppId;
        private readonly XsollaOptions m_Options;
        private readonly JsonHttp m_Http;

        /// <inheritdoc/>
        public override BillingPlatform Platform => BillingPlatform.Web;
        /// <inheritdoc/>
        public override string Store => BillingStores.Xsolla;

        /// <param name="config">Server URL, request timeout and the app id of its <c>xsolla</c> build.</param>
        /// <param name="storage">Remembers unpaid orders across launches (default: PlayerPrefs).</param>
        public XsollaStoreAdapter(BillingConfig config, XsollaOptions options = null, IHttpTransport transport = null,
            IBillingStorage storage = null, BillingLog log = null)
            : this(config ? config.Resolved().serverBaseUrl : null, config ? config.Resolved().AppIdFor(BillingStores.Xsolla) : null,
                config ? config.Resolved().requestTimeoutSeconds : BillingConfig.DefaultRequestTimeoutSeconds, options, transport, storage, log)
        {
        }

        /// <param name="serverBaseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        /// <param name="appId">App id of the Xsolla build on the server.</param>
        public XsollaStoreAdapter(string serverBaseUrl, string appId, int timeoutSeconds = BillingConfig.DefaultRequestTimeoutSeconds,
            XsollaOptions options = null,
            IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null)
            : base(storage, log, PendingKey, (options ?? new XsollaOptions()).ForgetAfter)
        {
            if (string.IsNullOrWhiteSpace(appId))
                throw new ArgumentException("The Xsolla build's app id is empty (an xsolla build in BillingConfig.builds)", nameof(appId));
            m_Http = new JsonHttp(serverBaseUrl, timeoutSeconds, transport);
            m_AppId = appId;
            m_Options = options ?? new XsollaOptions();
            m_AppLinkPrefix = AppLinkPrefix(m_Options.ReturnUrl);
            if (m_AppLinkPrefix != null)
                Application.deepLinkActivated += OnDeepLink;
        }

        private readonly string m_AppLinkPrefix;

        /// <summary><c>scheme://</c> of an app link return URL; null for a web page or none.</summary>
        internal static string AppLinkPrefix(string returnUrl)
        {
            if (string.IsNullOrEmpty(returnUrl) || returnUrl.StartsWith("http://", StringComparison.OrdinalIgnoreCase) ||
                returnUrl.StartsWith("https://", StringComparison.OrdinalIgnoreCase))
                return null;
            var i = returnUrl.IndexOf("://", StringComparison.Ordinal);
            return i > 0 ? returnUrl.Substring(0, i + 3) : null;
        }

        /// <summary>Pay Station sent the player back to the game: check the order now.</summary>
        internal void OnDeepLink(string url)
        {
            if (m_AppLinkPrefix != null && url != null && url.StartsWith(m_AppLinkPrefix, StringComparison.OrdinalIgnoreCase))
                CheckNow();
        }

        /// <inheritdoc/>
        public override void Dispose()
        {
            if (m_AppLinkPrefix != null)
                Application.deepLinkActivated -= OnDeepLink;
            base.Dispose();
        }

        /// <summary>
        /// Moves the items of the old client-side Xsolla integration (device-id login, Xsolla inventory) to
        /// <paramref name="userId"/>: the server reads the inventory with the player's Xsolla Login token and records
        /// the non-consumables of this build as owned (not as new sales). Call it once in a version that still has the
        /// old login; <see cref="IBillingService.ClaimXsollaPurchasesAsync"/> also refreshes the customer info.
        /// </summary>
        /// <exception cref="BillingException">The server refused the token or could not ask Xsolla.</exception>
        public async Task<XsollaClaimResponse> ClaimInventoryAsync(string userId, string xsollaLoginToken, CancellationToken ct = default)
        {
            if (string.IsNullOrEmpty(userId))
                throw new ArgumentException("user id is required", nameof(userId));
            if (string.IsNullOrEmpty(xsollaLoginToken))
                throw new ArgumentException("the Xsolla Login token is required", nameof(xsollaLoginToken));
            var request = new ClaimRequest { AppId = m_AppId, UserId = userId, XsollaToken = xsollaLoginToken };
            var res = await m_Http.PostAsync<XsollaClaimResponse>("/claim/xsolla", request, userId, ct);
            if (!res.IsHttpSuccess || res.Value == null)
                throw new BillingException("Xsolla inventory was not moved: " + (res.Value?.Message ?? res.Error ?? "no answer"));
            return res.Value;
        }

        protected override bool IsSold(BillingProductConfig product) => product.type != BillingProductType.Subscription;

        protected override async Task StartCheckoutAsync(string sku, string userId)
        {
            var request = new CheckoutRequest
            {
                AppId = m_AppId,
                ProductId = sku,
                UserId = userId,
                Sandbox = m_Options.Sandbox,
                ReturnUrl = NullIfEmpty(m_Options.ReturnUrl),
                Country = NullIfEmpty(m_Options.Country),
                Language = NullIfEmpty(m_Options.Language),
                Attribution = Checkout?.Attribution?.Count > 0 ? Checkout.Attribution : null,
            };
            var res = await m_Http.PostAsync<XsollaCheckoutResponse>("/checkout/xsolla", request, userId, Lifetime);
            var checkout = res.Value;
            if (!res.IsHttpSuccess || string.IsNullOrEmpty(checkout?.Url) || string.IsNullOrEmpty(checkout.OrderId))
            {
                Fail(sku, "checkout: " + (checkout?.Message ?? res.Error ?? "no answer"));
                return;
            }
            if (!IsSafePage(sku, checkout.Url))
                return;
            var order = Remember(checkout.OrderId, sku, userId);
            (m_Options.OpenUrl ?? PurserBrowser.Open)(checkout.Url);
            RaisePending(ToOrder(order, checkout));
        }

        /// <summary>Polls <c>/checkout/xsolla/status</c> until the order is granted, refunded or the payment timeout
        /// passes. The attribution went with the checkout.</summary>
        public Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, VerifyRequest request, CancellationToken ct)
        {
            var user = BuyerOf(order);
            if (user == null)
                return Task.FromResult(Answer(VerifyStatus.PendingOrCanceled, "the order belongs to another user"));
            var status = new StatusRequest { AppId = m_AppId, UserId = user, OrderId = order.TransactionId };
            return CheckAsync(order, token => m_Http.PostAsync<VerifyResponse>("/checkout/xsolla/status", status, user, token),
                m_Options.PollInterval, m_Options.PaymentTimeout, null, ct);
        }

        /// <inheritdoc/>
        public override void SetPlayerTokenProvider(IPlayerTokenProvider provider)
        {
            base.SetPlayerTokenProvider(provider);
            m_Http.Tokens = provider;
        }
    }
}
