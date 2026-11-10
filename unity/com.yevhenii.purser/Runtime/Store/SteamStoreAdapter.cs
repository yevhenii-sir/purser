using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>
    /// The parts of the Steamworks API the Steam adapter needs. The package does not depend on a Steamworks wrapper:
    /// the game provides this bridge (a ready-made one for Steamworks.NET is in the "Steam" sample).
    /// <see cref="AuthorizationReceived"/> must be raised on the Unity main thread (Steamworks.NET runs callbacks
    /// from <c>SteamAPI.RunCallbacks</c>, which games call in <c>Update</c>).
    /// </summary>
    public interface ISteamMicroTxnBridge
    {
        /// <summary>The Steam API is initialized and the player is logged on.</summary>
        bool IsReady { get; }

        /// <summary>SteamID64 of the player (<c>ISteamUser::GetSteamID</c>).</summary>
        ulong SteamId { get; }

        /// <summary>Game language (<c>ISteamApps::GetCurrentGameLanguage</c>, e.g. <c>english</c>).</summary>
        string Language { get; }

        /// <summary><c>MicroTxnAuthorizationResponse_t</c>: (app id, order id, authorized).</summary>
        event Action<uint, ulong, bool> AuthorizationReceived;
    }

    /// <summary>Settings of <see cref="SteamStoreAdapter"/>.</summary>
    public sealed class SteamOptions
    {
        /// <summary>How long to wait for the player's answer in the Steam overlay before reporting the purchase as
        /// failed. The server forgets unanswered orders by itself.</summary>
        public TimeSpan AuthorizationTimeout = TimeSpan.FromMinutes(10);

        /// <summary>Authorized orders the server has not answered for are re-checked at launch for this long.</summary>
        public TimeSpan ForgetAfter = TimeSpan.FromDays(2);
    }

    /// <summary>Answer of <c>POST /checkout/steam</c>.</summary>
    public sealed class SteamCheckoutResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        /// <summary>Order id (64-bit, as a string) Steam reports back in <c>MicroTxnAuthorizationResponse_t</c>.</summary>
        [JsonProperty("order_id")] public string OrderId;
        [JsonProperty("trans_id")] public string TransId;
        /// <summary>Currency charged (the player's wallet currency, or the catalog currency Steam converts).</summary>
        [JsonProperty("currency")] public string Currency;
        /// <summary>Amount in hundredths of <see cref="Currency"/>.</summary>
        [JsonProperty("amount")] public long Amount;
        /// <summary><c>production</c> or <c>sandbox</c> (<c>ISteamMicroTxnSandbox</c>).</summary>
        [JsonProperty("environment")] public string Environment;
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for Steam microtransactions. The billing server owns the publisher Web API key:
    /// a purchase asks it to start the order (<c>/checkout/steam</c>: <c>GetUserInfo</c> + <c>InitTxn</c>), Steam
    /// shows its overlay, and once the player authorized it (<c>MicroTxnAuthorizationResponse_t</c>) the order goes
    /// through <c>/verify/steam</c> like any store purchase (the server calls <c>FinalizeTxn</c>).
    /// <code>new BillingDependencies { Store = new SteamStoreAdapter(config, new SteamworksNetBridge()) }</code>
    /// Set <see cref="BillingConfig.steamAppId"/> to the Steam App ID of the build on the server. The product's store id
    /// is the billing product id; prices are set on the server (the overlay shows them).
    /// </summary>
    public sealed class SteamStoreAdapter : IStoreAdapter, IBillingUserStoreAdapter
    {
        public const string Store = "steam";
        private const string PendingKey = "purser.steam.authorized";

        private sealed class PendingOrder
        {
            [JsonProperty("order_id")] public ulong OrderId;
            [JsonProperty("sku")] public string Sku;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("created_ticks")] public long CreatedTicks;
        }

        private sealed class CheckoutRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("product_id")] public string ProductId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("steam_id")] public string SteamId;
            [JsonProperty("language", NullValueHandling = NullValueHandling.Ignore)] public string Language;
        }

        private readonly string m_BaseUrl;
        private readonly string m_AppId;
        private readonly int m_TimeoutSeconds;
        private readonly ISteamMicroTxnBridge m_Bridge;
        private readonly SteamOptions m_Options;
        private readonly IHttpTransport m_Transport;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly Func<TimeSpan, CancellationToken, Task> m_Delay;
        private readonly Func<DateTime> m_UtcNow;
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        /// <summary>Checked out, waiting for the player's answer in the overlay.</summary>
        private readonly Dictionary<ulong, PendingOrder> m_Awaiting = new Dictionary<ulong, PendingOrder>();
        /// <summary>Authorized, not yet answered by the server (persisted).</summary>
        private List<PendingOrder> m_Authorized;
        private string m_User;
        private bool m_Disposed;

        public BillingPlatform Platform => BillingPlatform.Steam;

        public event Action<StoreOrder> OrderPending;
        /// <summary>Steam has no deferred purchases.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        public event Action<StorePurchaseFailure> PurchaseFailed;
        public event Action<StoreOrder, bool, string> OrderConfirmed;

        /// <param name="config">Server URL, request timeout and the Steam build's app id (<see cref="BillingConfig.steamAppId"/>).</param>
        /// <param name="bridge">Steamworks glue (see the "Steam" sample).</param>
        /// <param name="storage">Remembers authorized orders across launches (default: PlayerPrefs).</param>
        public SteamStoreAdapter(BillingConfig config, ISteamMicroTxnBridge bridge, SteamOptions options = null,
            IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null)
            : this(config ? config.serverBaseUrl : null, config ? config.AppIdFor(BillingPlatform.Steam) : null, bridge,
                config ? config.requestTimeoutSeconds : 10, options, transport, storage, log)
        {
        }

        /// <param name="serverBaseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        /// <param name="appId">App id of the Steam build on the server (the Steam App ID).</param>
        public SteamStoreAdapter(string serverBaseUrl, string appId, ISteamMicroTxnBridge bridge, int timeoutSeconds = 10,
            SteamOptions options = null, IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null,
            Func<TimeSpan, CancellationToken, Task> delay = null, Func<DateTime> utcNow = null)
        {
            if (string.IsNullOrWhiteSpace(serverBaseUrl))
                throw new ArgumentException("Billing server URL is empty", nameof(serverBaseUrl));
            if (string.IsNullOrWhiteSpace(appId))
                throw new ArgumentException("The Steam build's app id is empty (BillingConfig.steamAppId)", nameof(appId));
            m_Bridge = bridge ?? throw new ArgumentNullException(nameof(bridge));
            m_BaseUrl = serverBaseUrl.TrimEnd('/');
            m_AppId = appId;
            m_TimeoutSeconds = Math.Max(1, timeoutSeconds);
            m_Options = options ?? new SteamOptions();
            m_Transport = transport ?? new UnityWebRequestTransport();
            m_Storage = storage ?? new PlayerPrefsBillingStorage();
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
            m_Delay = delay ?? ((d, ct) => Task.Delay(d, ct));
            m_UtcNow = utcNow ?? (() => DateTime.UtcNow);
            m_Bridge.AuthorizationReceived += HandleAuthorization;
        }

        /// <summary>Re-raises orders the player authorized in an earlier session that the server has not answered yet.</summary>
        public Task ConnectAsync(CancellationToken ct)
        {
            if (!m_Bridge.IsReady)
                throw new BillingException("Steam is not running or the player is not logged on (SteamAPI.Init)");
            var cutoff = m_UtcNow() - m_Options.ForgetAfter;
            if (Authorized.RemoveAll(o => new DateTime(o.CreatedTicks, DateTimeKind.Utc) < cutoff) > 0)
                SaveAuthorized();
            foreach (var o in Authorized.ToList())
                OrderPending?.Invoke(ToOrder(o));
            return Task.CompletedTask;
        }

        /// <summary>Products from the config: Steam shows the price in its overlay (set on the server).</summary>
        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            IReadOnlyList<StoreProduct> list = products
                .Select(p => new StoreProduct(p.id, p.StoreId(Platform), p.type, "", 0m, null, p.id, "", true))
                .ToList();
            return Task.FromResult(list);
        }

        /// <summary>Steam has no account binding field: the server binds the order to the billing user id.</summary>
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
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Steam: the billing user id is not set", false));
                    return;
                }
                if (!m_Bridge.IsReady || m_Bridge.SteamId == 0)
                {
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Steam is not running or the player is not logged on", false));
                    return;
                }
                var request = new CheckoutRequest
                {
                    AppId = m_AppId,
                    ProductId = sku,
                    UserId = user,
                    SteamId = m_Bridge.SteamId.ToString(CultureInfo.InvariantCulture),
                    Language = string.IsNullOrEmpty(m_Bridge.Language) ? null : m_Bridge.Language,
                };
                var res = await m_Transport.SendAsync("POST", m_BaseUrl + "/checkout/steam", JsonConvert.SerializeObject(request),
                    m_TimeoutSeconds, m_Lifetime.Token);
                SteamCheckoutResponse checkout = null;
                try
                {
                    if (!res.IsTransportError && !string.IsNullOrEmpty(res.Body))
                        checkout = JsonConvert.DeserializeObject<SteamCheckoutResponse>(res.Body);
                }
                catch (JsonException)
                {
                    // Reported below.
                }
                if (res.Status < 200 || res.Status >= 300 || checkout == null ||
                    !ulong.TryParse(checkout.OrderId, NumberStyles.None, CultureInfo.InvariantCulture, out var orderId))
                {
                    var reason = checkout?.Message ?? res.Error ?? "HTTP " + res.Status;
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Steam checkout: " + reason, false));
                    return;
                }
                // Steam now shows the overlay; the answer arrives as MicroTxnAuthorizationResponse_t.
                m_Awaiting[orderId] = new PendingOrder { OrderId = orderId, Sku = sku, UserId = user, CreatedTicks = m_UtcNow().Ticks };
                _ = ExpireAsync(orderId);
            }
            catch (OperationCanceledException)
            {
                // Disposed.
            }
            catch (Exception e)
            {
                m_Log.Error("Steam checkout failed: " + e);
                PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Steam checkout: " + e.Message, false));
            }
        }

        private async Task ExpireAsync(ulong orderId)
        {
            try
            {
                await m_Delay(m_Options.AuthorizationTimeout, m_Lifetime.Token);
            }
            catch (OperationCanceledException)
            {
                return;
            }
            if (!m_Awaiting.TryGetValue(orderId, out var order))
                return;
            m_Awaiting.Remove(orderId);
            PurchaseFailed?.Invoke(new StorePurchaseFailure(order.Sku, "Steam: no answer from the purchase overlay", false));
        }

        private void HandleAuthorization(uint appId, ulong orderId, bool authorized)
        {
            m_Awaiting.TryGetValue(orderId, out var order);
            m_Awaiting.Remove(orderId);
            if (!authorized)
            {
                if (order != null)
                    PurchaseFailed?.Invoke(new StorePurchaseFailure(order.Sku, "Steam: the player declined the purchase", true));
                return;
            }
            if (order == null)
            {
                // Started before a restart of the game: the server knows its product.
                if (Authorized.Any(o => o.OrderId == orderId) || string.IsNullOrEmpty(m_User))
                    return;
                order = new PendingOrder { OrderId = orderId, Sku = "", UserId = m_User, CreatedTicks = m_UtcNow().Ticks };
            }
            if (Authorized.All(o => o.OrderId != orderId))
            {
                Authorized.Add(order);
                SaveAuthorized();
            }
            OrderPending?.Invoke(ToOrder(order));
        }

        /// <summary>The server answered for the order: it is no longer re-checked at launch.</summary>
        public void Confirm(StoreOrder order)
        {
            if (order?.TransactionId == null)
                return;
            if (Authorized.RemoveAll(o => o.OrderId.ToString(CultureInfo.InvariantCulture) == order.TransactionId) > 0)
                SaveAuthorized();
            OrderConfirmed?.Invoke(order, true, null);
        }

        /// <summary>Owned products come from the server's entitlements: Steam has nothing to restore.</summary>
        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<StoreOrder>>(new List<StoreOrder>());

        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        public void Dispose()
        {
            if (m_Disposed)
                return;
            m_Disposed = true;
            m_Bridge.AuthorizationReceived -= HandleAuthorization;
            m_Lifetime.Cancel();
            m_Lifetime.Dispose();
        }

        // ---- helpers ----

        private List<PendingOrder> Authorized => m_Authorized ?? (m_Authorized = LoadAuthorized());

        private StoreOrder ToOrder(PendingOrder o)
        {
            var id = o.OrderId.ToString(CultureInfo.InvariantCulture);
            return new StoreOrder(Platform, o.Sku, id, null, id, null, o,
                m_Bridge.SteamId == 0 ? null : m_Bridge.SteamId.ToString(CultureInfo.InvariantCulture));
        }

        private List<PendingOrder> LoadAuthorized()
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

        private void SaveAuthorized()
        {
            try
            {
                m_Storage.Save(PendingKey, JsonConvert.SerializeObject(Authorized));
            }
            catch (Exception e)
            {
                m_Log.Warning("Could not remember Steam orders: " + e.Message);
            }
        }
    }
}
