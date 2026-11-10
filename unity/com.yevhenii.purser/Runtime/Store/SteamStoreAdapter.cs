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
    /// The app id is the <c>steam</c> build of <see cref="BillingConfig.builds"/> (the Steam App ID). The product's
    /// store id is the billing product id; prices are set on the server (the overlay shows them).
    /// </summary>
    public sealed class SteamStoreAdapter : ServerCheckoutStoreAdapter
    {
        private const string PendingKey = "purser.steam.authorized";

        private sealed class CheckoutRequest
        {
            [JsonProperty("app_id")] public string AppId;
            [JsonProperty("product_id")] public string ProductId;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("steam_id")] public string SteamId;
            [JsonProperty("language", NullValueHandling = NullValueHandling.Ignore)] public string Language;
        }

        private readonly string m_AppId;
        private readonly ISteamMicroTxnBridge m_Bridge;
        private readonly SteamOptions m_Options;
        private readonly JsonHttp m_Http;
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        /// <summary>Checked out, waiting for the player's answer in the overlay (not persisted).</summary>
        private readonly Dictionary<string, (string Sku, string User)> m_Awaiting = new Dictionary<string, (string, string)>();
        private bool m_Disposed;

        public override BillingPlatform Platform => BillingPlatform.Steam;
        public override string Store => BillingStores.Steam;

        /// <param name="config">Server URL, request timeout and the app id of its <c>steam</c> build.</param>
        /// <param name="bridge">Steamworks glue (see the "Steam" sample).</param>
        /// <param name="storage">Remembers authorized orders across launches (default: PlayerPrefs).</param>
        public SteamStoreAdapter(BillingConfig config, ISteamMicroTxnBridge bridge, SteamOptions options = null,
            IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null)
            : this(config ? config.serverBaseUrl : null, config ? config.AppIdFor(BillingStores.Steam) : null, bridge,
                config ? config.requestTimeoutSeconds : 10, options, transport, storage, log)
        {
        }

        /// <param name="serverBaseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        /// <param name="appId">App id of the Steam build on the server (the Steam App ID).</param>
        public SteamStoreAdapter(string serverBaseUrl, string appId, ISteamMicroTxnBridge bridge, int timeoutSeconds = 10,
            SteamOptions options = null, IHttpTransport transport = null, IBillingStorage storage = null, BillingLog log = null)
            : base(storage, log, PendingKey, (options ?? new SteamOptions()).ForgetAfter)
        {
            if (string.IsNullOrWhiteSpace(appId))
                throw new ArgumentException("The Steam build's app id is empty (a steam build in BillingConfig.builds)", nameof(appId));
            m_Bridge = bridge ?? throw new ArgumentNullException(nameof(bridge));
            m_Http = new JsonHttp(serverBaseUrl, timeoutSeconds, transport);
            m_AppId = appId;
            m_Options = options ?? new SteamOptions();
            m_Bridge.AuthorizationReceived += HandleAuthorization;
        }

        /// <summary>Re-raises orders the player authorized in an earlier session that the server has not answered yet.</summary>
        public override Task ConnectAsync(CancellationToken ct)
        {
            if (!m_Bridge.IsReady)
                throw new BillingException("Steam is not running or the player is not logged on (SteamAPI.Init)");
            return base.ConnectAsync(ct);
        }

        protected override async Task StartCheckoutAsync(string sku, string userId)
        {
            if (!m_Bridge.IsReady || m_Bridge.SteamId == 0)
            {
                Fail(sku, "Steam is not running or the player is not logged on");
                return;
            }
            var request = new CheckoutRequest
            {
                AppId = m_AppId,
                ProductId = sku,
                UserId = userId,
                SteamId = m_Bridge.SteamId.ToString(CultureInfo.InvariantCulture),
                Language = NullIfEmpty(m_Bridge.Language),
            };
            var res = await m_Http.PostAsync<SteamCheckoutResponse>("/checkout/steam", request, m_Lifetime.Token);
            var checkout = res.Value;
            if (!res.IsHttpSuccess || checkout == null ||
                !ulong.TryParse(checkout.OrderId, NumberStyles.None, CultureInfo.InvariantCulture, out var orderId))
            {
                Fail(sku, "checkout: " + (checkout?.Message ?? res.Error ?? "HTTP " + res.HttpStatus));
                return;
            }
            // Steam now shows the overlay; the answer arrives as MicroTxnAuthorizationResponse_t.
            var id = orderId.ToString(CultureInfo.InvariantCulture);
            m_Awaiting[id] = (sku, userId);
            _ = ExpireAsync(id);
        }

        private async Task ExpireAsync(string orderId)
        {
            try
            {
                await Delay(m_Options.AuthorizationTimeout, m_Lifetime.Token);
            }
            catch (OperationCanceledException)
            {
                return;
            }
            if (!m_Awaiting.TryGetValue(orderId, out var started))
                return;
            m_Awaiting.Remove(orderId);
            Fail(started.Sku, "no answer from the purchase overlay");
        }

        private void HandleAuthorization(uint appId, ulong steamOrderId, bool authorized)
        {
            var orderId = steamOrderId.ToString(CultureInfo.InvariantCulture);
            var isStarted = m_Awaiting.TryGetValue(orderId, out var started);
            m_Awaiting.Remove(orderId);
            if (!authorized)
            {
                if (isStarted)
                    Fail(started.Sku, "the player declined the purchase", true);
                return;
            }
            var known = Pending.Any(o => o.OrderId == orderId);
            if (!isStarted && (known || CurrentUser == null))
                return;
            // Started before a restart of the game: the sku is unknown, the server's answer names the product.
            var order = isStarted ? Remember(orderId, started.Sku, started.User) : Remember(orderId, "", CurrentUser);
            RaisePending(ToOrder(order));
        }

        /// <summary>Steam orders are looked up by the player's SteamID.</summary>
        protected override StoreOrder ToOrder(CheckoutOrder o, object native = null) =>
            new StoreOrder(Platform, o.Sku, o.OrderId, null, native ?? o,
                m_Bridge.SteamId == 0 ? null : m_Bridge.SteamId.ToString(CultureInfo.InvariantCulture));

        public override void Dispose()
        {
            if (m_Disposed)
                return;
            m_Disposed = true;
            m_Bridge.AuthorizationReceived -= HandleAuthorization;
            m_Lifetime.Cancel();
            m_Lifetime.Dispose();
        }
    }
}
