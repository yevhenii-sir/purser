using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>How Telegram closed an invoice (<c>invoiceClosed</c> of a Mini App).</summary>
    public static class TelegramInvoiceStatus
    {
        /// <summary>Paid: the server grants it once Telegram's <c>successful_payment</c> reached it.</summary>
        public const string Paid = "paid";
        /// <summary>The player closed the invoice without paying.</summary>
        public const string Cancelled = "cancelled";
        /// <summary>The payment failed.</summary>
        public const string Failed = "failed";
        /// <summary>The payment is still processing.</summary>
        public const string Pending = "pending";
    }

    /// <summary>
    /// The Mini App call the adapter needs: <c>Telegram.WebApp.openInvoice(url, callback)</c>. The "Telegram Mini App"
    /// sample implements it with a jslib. <paramref name="closed"/> gets a <see cref="TelegramInvoiceStatus"/> on the
    /// Unity main thread.
    /// </summary>
    public interface ITelegramWebApp
    {
        /// <summary>Opens the invoice; <paramref name="closed"/> runs when Telegram closes it.</summary>
        void OpenInvoice(string url, Action<string> closed);
    }

    /// <summary>Settings of <see cref="TelegramStoreAdapter"/>.</summary>
    public sealed class TelegramOptions
    {
        /// <summary>How often the order status is asked while the payment is processed.</summary>
        public TimeSpan PollInterval = TimeSpan.FromSeconds(2);

        /// <summary>How long to wait for the payment before reporting <see cref="PurchaseStatus.PendingPayment"/>.</summary>
        public TimeSpan PaymentTimeout = TimeSpan.FromMinutes(5);

        /// <summary>Orders older than this are forgotten (the server forgets unpaid ones after 7 days).</summary>
        public TimeSpan ForgetAfter = TimeSpan.FromDays(7);
    }

    /// <summary>Answer of <c>POST /checkout/telegram</c>.</summary>
    public sealed class TelegramCheckoutResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        /// <summary>The invoice link to open.</summary>
        [JsonProperty("url")] public string Url;
        /// <summary>The order (the invoice payload).</summary>
        [JsonProperty("order_id")] public string OrderId;
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for Telegram Stars in a Telegram Mini App. Purchase: the billing server creates an
    /// invoice link of the game's bot (<c>/checkout/telegram</c>), the adapter opens it with
    /// <see cref="ITelegramWebApp.OpenInvoice"/>, and the order is polled (<c>/checkout/telegram/status</c>) until the
    /// bot's webhook reached the server and the reward was granted. Products are priced in Stars on the server
    /// (<c>XTR</c>); subscriptions renew every 30 days.
    /// <code>new BillingDependencies { Store = new TelegramStoreAdapter(config, new TelegramWebAppBridge()) }</code>
    /// The app id is the <c>telegram</c> build of <see cref="BillingConfig.builds"/>.
    /// </summary>
    public sealed class TelegramStoreAdapter : ServerCheckoutStoreAdapter, IWebhookStoreAdapter
    {
        private const string PendingKey = "purser.telegram.pending";

        private sealed class CheckoutRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("product_id")] public string ProductId;
            [JsonProperty("user_id")] public string UserId;
        }

        private sealed class StatusRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("order_id")] public string OrderId;
        }

        private readonly string m_AppId;
        private readonly ITelegramWebApp m_WebApp;
        private readonly TelegramOptions m_Options;
        private readonly JsonHttp m_Http;
        /// <summary>Orders whose invoice the player closed without paying.</summary>
        private readonly HashSet<string> m_Closed = new HashSet<string>();

        /// <inheritdoc/>
        public override BillingPlatform Platform => BillingPlatform.Web;
        /// <inheritdoc/>
        public override string Store => BillingStores.Telegram;

        /// <param name="config">Server URL, request timeout and the app id of its <c>telegram</c> build.</param>
        /// <param name="webApp">Opens invoices in the Mini App.</param>
        /// <param name="storage">Remembers unpaid orders across launches (default: PlayerPrefs).</param>
        public TelegramStoreAdapter(BillingConfig config, ITelegramWebApp webApp, TelegramOptions options = null, IHttpTransport transport = null,
            IBillingStorage storage = null, BillingLog log = null)
            : this(config ? config.Resolved().serverBaseUrl : null, config ? config.Resolved().AppIdFor(BillingStores.Telegram) : null, webApp,
                config ? config.Resolved().requestTimeoutSeconds : BillingConfig.DefaultRequestTimeoutSeconds, options, transport, storage, log)
        {
        }

        /// <param name="serverBaseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        /// <param name="appId">App id of the Telegram build on the server.</param>
        public TelegramStoreAdapter(string serverBaseUrl, string appId, ITelegramWebApp webApp, int timeoutSeconds = BillingConfig.DefaultRequestTimeoutSeconds,
            TelegramOptions options = null, IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null)
            : base(storage, log, PendingKey, (options ?? new TelegramOptions()).ForgetAfter)
        {
            if (string.IsNullOrWhiteSpace(appId))
                throw new ArgumentException("The Telegram build's app id is empty (a telegram build in BillingConfig.builds)", nameof(appId));
            m_WebApp = webApp ?? throw new ArgumentNullException(nameof(webApp));
            m_Http = new JsonHttp(serverBaseUrl, timeoutSeconds, transport);
            m_AppId = appId;
            m_Options = options ?? new TelegramOptions();
        }

        protected override async Task StartCheckoutAsync(string sku, string userId)
        {
            var request = new CheckoutRequest { AppId = m_AppId, ProductId = sku, UserId = userId };
            var res = await m_Http.PostAsync<TelegramCheckoutResponse>("/checkout/telegram", request, userId, Lifetime);
            var checkout = res.Value;
            if (!res.IsHttpSuccess || string.IsNullOrEmpty(checkout?.Url) || string.IsNullOrEmpty(checkout.OrderId))
            {
                Fail(sku, "checkout: " + (checkout?.Message ?? res.Error ?? "no answer"));
                return;
            }
            if (!IsSafePage(sku, checkout.Url))
                return;
            var order = Remember(checkout.OrderId, sku, userId);
            var orderId = checkout.OrderId;
            m_WebApp.OpenInvoice(checkout.Url, status => OnInvoiceClosed(orderId, status));
            RaisePending(ToOrder(order, checkout));
        }

        /// <summary>Telegram closed the invoice: a payment is checked at once, a cancelled or failed one ends the
        /// order's polling.</summary>
        internal void OnInvoiceClosed(string orderId, string status)
        {
            if (status == TelegramInvoiceStatus.Cancelled || status == TelegramInvoiceStatus.Failed)
                m_Closed.Add(orderId);
            CheckNow();
        }

        /// <summary>Polls <c>/checkout/telegram/status</c> until the order is granted, refunded, the invoice was
        /// closed unpaid or the payment timeout passes.</summary>
        public Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, VerifyRequest request, CancellationToken ct)
        {
            var user = BuyerOf(order);
            if (user == null)
                return Task.FromResult(Answer(VerifyStatus.PendingOrCanceled, "the order belongs to another user"));
            var status = new StatusRequest { AppId = m_AppId, UserId = user, OrderId = order.TransactionId };
            var orderId = order.TransactionId;
            return CheckAsync(order, token => m_Http.PostAsync<VerifyResponse>("/checkout/telegram/status", status, user, token),
                m_Options.PollInterval, m_Options.PaymentTimeout, () => m_Closed.Contains(orderId), ct);
        }

        /// <inheritdoc/>
        public override void SetPlayerTokenProvider(IPlayerTokenProvider provider)
        {
            base.SetPlayerTokenProvider(provider);
            m_Http.Tokens = provider;
        }
    }
}
