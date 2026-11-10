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

        /// <summary>Opens the Pay Station page. Default: <c>Application.OpenURL</c> (system browser on desktop, a new
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
    /// The product's store id is the Xsolla item SKU. Subscriptions are not sold through this adapter.
    /// </summary>
    public sealed class XsollaStoreAdapter : IStoreAdapter, IStoreNameProvider, IWebhookStoreAdapter
    {
        public const string Store = "xsolla";
        private const string PendingKey = "purser.xsolla.pending";

        private sealed class PendingOrder
        {
            [JsonProperty("order_id")] public string OrderId;
            [JsonProperty("sku")] public string Sku;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("created_ticks")] public long CreatedTicks;
        }

        private sealed class CheckoutRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("product_id")] public string ProductId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("sandbox")] public bool Sandbox;
            [JsonProperty("return_url", NullValueHandling = NullValueHandling.Ignore)] public string ReturnUrl;
            [JsonProperty("country", NullValueHandling = NullValueHandling.Ignore)] public string Country;
            [JsonProperty("language", NullValueHandling = NullValueHandling.Ignore)] public string Language;
        }

        private sealed class StatusRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("order_id")] public string OrderId;
        }

        private readonly string m_BaseUrl;
        private readonly string m_AppId;
        private readonly int m_TimeoutSeconds;
        private readonly XsollaOptions m_Options;
        private readonly IHttpTransport m_Transport;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly Func<TimeSpan, CancellationToken, Task> m_Delay;
        private readonly Func<DateTime> m_UtcNow;
        private List<PendingOrder> m_Pending;
        private string m_User;

        public BillingPlatform Platform => BillingPlatform.Web;
        public string StoreName => Store;

        public event Action<StoreOrder> OrderPending;
        /// <summary>Not used: an unpaid order is reported by <see cref="CheckOrderAsync"/>.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        public event Action<StorePurchaseFailure> PurchaseFailed;
        public event Action<StoreOrder, bool, string> OrderConfirmed;

        /// <param name="config">Server URL, request timeout and the web build's app id (<see cref="BillingConfig.webAppId"/>).</param>
        /// <param name="storage">Remembers unpaid orders across launches (default: PlayerPrefs).</param>
        public XsollaStoreAdapter(BillingConfig config, XsollaOptions options = null, IHttpTransport transport = null,
            IBillingStorage storage = null, BillingLog log = null)
            : this(config ? config.serverBaseUrl : null, config ? config.AppIdFor(BillingPlatform.Web) : null,
                config ? config.requestTimeoutSeconds : 10, options, transport, storage, log)
        {
        }

        /// <param name="serverBaseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        /// <param name="appId">App id of the Xsolla build on the server.</param>
        public XsollaStoreAdapter(string serverBaseUrl, string appId, int timeoutSeconds = 10, XsollaOptions options = null,
            IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null,
            Func<TimeSpan, CancellationToken, Task> delay = null, Func<DateTime> utcNow = null)
        {
            if (string.IsNullOrWhiteSpace(serverBaseUrl))
                throw new ArgumentException("Billing server URL is empty", nameof(serverBaseUrl));
            if (string.IsNullOrWhiteSpace(appId))
                throw new ArgumentException("The Xsolla build's app id is empty (BillingConfig.webAppId)", nameof(appId));
            m_BaseUrl = serverBaseUrl.TrimEnd('/');
            m_AppId = appId;
            m_TimeoutSeconds = Math.Max(1, timeoutSeconds);
            m_Options = options ?? new XsollaOptions();
            m_Transport = transport ?? new UnityWebRequestTransport();
            m_Storage = storage ?? new PlayerPrefsBillingStorage();
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
            m_Delay = delay ?? ((d, ct) => Task.Delay(d, ct));
            m_UtcNow = utcNow ?? (() => DateTime.UtcNow);
        }

        /// <summary>Re-raises orders of earlier sessions that were not confirmed (paid after the app closed, or not yet).</summary>
        public Task ConnectAsync(CancellationToken ct)
        {
            m_Pending = LoadPending();
            var cutoff = m_UtcNow() - m_Options.ForgetAfter;
            var expired = m_Pending.RemoveAll(o => new DateTime(o.CreatedTicks, DateTimeKind.Utc) < cutoff);
            if (expired > 0)
                SavePending();
            foreach (var o in m_Pending.ToList())
                OrderPending?.Invoke(ToOrder(o));
            return Task.CompletedTask;
        }

        /// <summary>Checks the remembered unpaid orders again (e.g. when the player returns from Pay Station after
        /// <see cref="XsollaOptions.PaymentTimeout"/>); their results arrive as purchase events of the billing service.</summary>
        public void ResumePendingOrders()
        {
            foreach (var o in Pending.ToList())
                OrderPending?.Invoke(ToOrder(o));
        }

        /// <summary>Products from the config: prices are shown by Pay Station (the catalog lives in Xsolla).</summary>
        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            IReadOnlyList<StoreProduct> list = products
                .Select(p => new StoreProduct(p.id, p.StoreId(Platform), p.type, "", 0m, null, p.id, "",
                    p.type != BillingProductType.Subscription))
                .ToList();
            return Task.FromResult(list);
        }

        /// <summary>Xsolla has no account binding field: the server puts the billing user id into the payment token.</summary>
        public void SetAccountBinding(string googleAccountId, Guid? appleAccountToken) { }

        public void SetBillingUser(string userId) => m_User = userId;

        public void Purchase(string storeProductId) => _ = PurchaseCoreAsync(storeProductId);

        private async Task PurchaseCoreAsync(string sku)
        {
            try
            {
                var user = m_User;
                if (string.IsNullOrEmpty(user))
                {
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Xsolla: the billing user id is not set", false));
                    return;
                }
                var request = new CheckoutRequest
                {
                    AppId = m_AppId,
                    ProductId = sku,
                    UserId = user,
                    Sandbox = m_Options.Sandbox,
                    ReturnUrl = NullIfEmpty(m_Options.ReturnUrl),
                    Country = NullIfEmpty(m_Options.Country),
                    Language = NullIfEmpty(m_Options.Language),
                };
                var res = await PostAsync<XsollaCheckoutResponse>("/checkout/xsolla", request, CancellationToken.None);
                var checkout = res.Value;
                if (!res.IsHttpSuccess || string.IsNullOrEmpty(checkout?.Url) || string.IsNullOrEmpty(checkout.OrderId))
                {
                    var reason = checkout?.Message ?? res.Error ?? "no answer";
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Xsolla checkout: " + reason, false));
                    return;
                }
                var pending = new PendingOrder { OrderId = checkout.OrderId, Sku = sku, UserId = user, CreatedTicks = m_UtcNow().Ticks };
                Pending.Add(pending);
                SavePending();
                (m_Options.OpenUrl ?? Application.OpenURL)(checkout.Url);
                OrderPending?.Invoke(ToOrder(pending, checkout));
            }
            catch (Exception e)
            {
                m_Log.Error("Xsolla checkout failed: " + e);
                PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Xsolla checkout: " + e.Message, false));
            }
        }

        /// <summary>Polls <c>/checkout/xsolla/status</c> until the order is granted, refunded or the payment timeout passes.</summary>
        public async Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, CancellationToken ct)
        {
            var user = string.IsNullOrEmpty(order.StoreUserId) ? m_User : order.StoreUserId;
            if (!string.IsNullOrEmpty(m_User) && user != m_User)
                // Paid by another player of this device: granted to that player by the server; not reported here.
                return Answer(VerifyStatus.PendingOrCanceled, "the order belongs to another user");
            var request = new StatusRequest { AppId = m_AppId, UserId = user, OrderId = order.TransactionId };
            var deadline = m_UtcNow() + m_Options.PaymentTimeout;
            while (true)
            {
                var res = await PostAsync<VerifyResponse>("/checkout/xsolla/status", request, ct);
                var status = res.Value?.Status;
                var unpaid = status == VerifyStatus.PendingOrCanceled || res.IsTransportError || res.HttpStatus >= 500;
                if (!unpaid || m_UtcNow() >= deadline)
                    return res;
                await m_Delay(m_Options.PollInterval, ct);
            }
        }

        /// <summary>The server answered for the order: it is no longer checked at launch.</summary>
        public void Confirm(StoreOrder order)
        {
            if (order?.TransactionId == null)
                return;
            if (Pending.RemoveAll(o => o.OrderId == order.TransactionId) > 0)
                SavePending();
            OrderConfirmed?.Invoke(order, true, null);
        }

        /// <summary>Owned products come from the server's entitlements: nothing to restore from the store.</summary>
        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<StoreOrder>>(new List<StoreOrder>());

        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        public void Dispose() { }

        // ---- helpers ----

        private List<PendingOrder> Pending => m_Pending ?? (m_Pending = LoadPending());

        private StoreOrder ToOrder(PendingOrder o, object native = null) =>
            new StoreOrder(Platform, o.Sku, o.OrderId, null, o.OrderId, null, native, o.UserId);

        private static ApiResult<VerifyResponse> Answer(string status, string message) =>
            new ApiResult<VerifyResponse>(200, new VerifyResponse { Status = status, Message = message }, null);

        private static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;

        private async Task<ApiResult<T>> PostAsync<T>(string path, object body, CancellationToken ct) where T : class
        {
            var res = await m_Transport.SendAsync("POST", m_BaseUrl + path, JsonConvert.SerializeObject(body), m_TimeoutSeconds, ct);
            if (res.IsTransportError)
                return new ApiResult<T>(0, null, res.Error);
            T value = null;
            string error = null;
            try
            {
                if (!string.IsNullOrEmpty(res.Body))
                    value = JsonConvert.DeserializeObject<T>(res.Body);
            }
            catch (JsonException e)
            {
                error = "invalid JSON from server: " + e.Message;
            }
            if (error == null && (res.Status < 200 || res.Status >= 300))
                error = "HTTP " + res.Status;
            return new ApiResult<T>(res.Status, value, error);
        }

        private List<PendingOrder> LoadPending()
        {
            try
            {
                var json = m_Storage.Load(PendingKey);
                return string.IsNullOrEmpty(json)
                    ? new List<PendingOrder>()
                    : JsonConvert.DeserializeObject<List<PendingOrder>>(json) ?? new List<PendingOrder>();
            }
            catch (Exception)
            {
                return new List<PendingOrder>();
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
                m_Log.Warning("Could not remember Xsolla orders: " + e.Message);
            }
        }
    }
}
