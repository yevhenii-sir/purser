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

        /// <summary>Where Pay Station sends the player after paying (optional, http(s)), e.g. the game's page.</summary>
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

        /// <summary>Opens the Pay Station page (only https pages are opened). Default: <c>Application.OpenURL</c> (system browser on desktop, a new
        /// tab on WebGL). On WebGL a page opened after an await may be blocked as a popup: open it from a jslib
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

    /// <summary>
    /// <see cref="IStoreAdapter"/> for Xsolla Pay Station (web, desktop). Purchase: the billing server creates a payment
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
            : this(config ? config.serverBaseUrl : null, config ? config.AppIdFor(BillingStores.Xsolla) : null,
                config ? config.requestTimeoutSeconds : BillingConfig.DefaultRequestTimeoutSeconds, options, transport, storage, log)
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
            var res = await m_Http.PostAsync<XsollaCheckoutResponse>("/checkout/xsolla", request, Lifetime);
            var checkout = res.Value;
            if (!res.IsHttpSuccess || string.IsNullOrEmpty(checkout?.Url) || string.IsNullOrEmpty(checkout.OrderId))
            {
                Fail(sku, "checkout: " + (checkout?.Message ?? res.Error ?? "no answer"));
                return;
            }
            if (!IsSafePage(sku, checkout.Url))
                return;
            var order = Remember(checkout.OrderId, sku, userId);
            (m_Options.OpenUrl ?? Application.OpenURL)(checkout.Url);
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
            return PollAsync(token => m_Http.PostAsync<VerifyResponse>("/checkout/xsolla/status", status, token),
                m_Options.PollInterval, m_Options.PaymentTimeout, null, ct);
        }
    }
}
