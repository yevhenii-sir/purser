using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Purser
{
    /// <summary>
    /// The Google Play Billing Library calls behind <see cref="GooglePlayOfferClient"/>: the package's Android plugin
    /// (<see cref="AndroidGooglePlayBillingBridge"/>) or a fake in tests. Answers are JSON, passed to the callback once,
    /// on any thread:
    /// <c>{"code": 0, "message": "", "offers": [{"basePlanId", "offerId", "offerToken", "tags": [], "phases":
    /// [{"priceMicros", "formattedPrice", "currency", "period", "cycles", "recurrenceMode"}]}]}</c> for
    /// <see cref="QueryOffers"/> and <c>{"code": 0, "message": "", "purchases": [{"purchaseToken", "orderId",
    /// "products": [], "state", "acknowledged"}]}</c> for <see cref="Purchase"/>. <c>code</c> is Google's
    /// <c>BillingResponseCode</c> (-100: an error of the plugin).
    /// </summary>
    public interface IGooglePlayBillingBridge
    {
        /// <summary>The Billing Library and the plugin are in the app (an Android device, not the Editor).</summary>
        bool IsAvailable { get; }

        /// <summary>Queries the subscription's <c>ProductDetails</c> and answers its offers.</summary>
        void QueryOffers(string productId, Action<string> onResult);

        /// <summary>Launches the billing flow with the offer and answers the purchases Google Play reports. Never
        /// acknowledges or consumes them.</summary>
        void Purchase(GooglePlayBillingFlow flow, Action<string> onResult);
    }

    /// <summary>One purchase of a Google Play subscription offer (<see cref="IGooglePlayBillingBridge.Purchase"/>).</summary>
    public sealed class GooglePlayBillingFlow
    {
        /// <summary>The subscription's product id in Google Play.</summary>
        public string ProductId;

        /// <summary>The offer token (<see cref="StoreOffer.Token"/>).</summary>
        public string OfferToken;

        /// <summary>A subscription of the player this purchase replaces (its product id), or null.</summary>
        public string OldProductId;

        /// <summary>How the replacement of <see cref="OldProductId"/> takes effect.</summary>
        public ReplacementMode ReplacementMode = ReplacementMode.WithTimeProration;

        /// <summary>Google <c>obfuscatedAccountId</c> of the player (<see cref="AccountIds.GoogleAccountId"/>), or null.</summary>
        public string ObfuscatedAccountId;
    }

    /// <summary>
    /// A Google Play purchase made with <see cref="GooglePlayOfferClient"/>: the <see cref="StoreOrder.Native"/> of its
    /// order. It is verified by the server like any other order, and the server acknowledges it: nothing finishes it on
    /// the device.
    /// </summary>
    public sealed class GooglePlayPurchase
    {
        /// <summary>The purchase token.</summary>
        public string PurchaseToken { get; }

        /// <summary>Google's order id (<c>GPA.…</c>); null for some test purchases.</summary>
        public string OrderId { get; }

        /// <summary>The product ids of the purchase.</summary>
        public IReadOnlyList<string> ProductIds { get; }

        /// <summary>The payment is pending (Google <c>PurchaseState.PENDING</c>).</summary>
        public bool IsPending { get; }

        /// <summary>The purchase was acknowledged when Google Play reported it (never by the package).</summary>
        public bool IsAcknowledged { get; }

        /// <summary>The offer token the purchase was made with.</summary>
        public string OfferToken { get; }

        /// <summary>Creates a purchase (tests).</summary>
        public GooglePlayPurchase(string purchaseToken, string orderId, IReadOnlyList<string> productIds, bool isPending,
            bool isAcknowledged, string offerToken)
        {
            PurchaseToken = purchaseToken;
            OrderId = string.IsNullOrEmpty(orderId) ? null : orderId;
            ProductIds = productIds ?? new string[0];
            IsPending = isPending;
            IsAcknowledged = isAcknowledged;
            OfferToken = offerToken;
        }
    }

    /// <summary>
    /// Google Play subscription offers through the Billing Library, next to Unity IAP (which buys only the first offer
    /// of a product): lists the offers of a subscription and buys one by its offer token. The purchase is reported as a
    /// <see cref="StoreOrder"/> (<see cref="OrderPending"/>) with the purchase token and order id, so the store adapter
    /// hands it to the server's <c>/verify/google</c> like any other; the server acknowledges it. Used by
    /// <c>UnityIapStoreAdapter</c>; another <see cref="IStoreAdapter"/> may use it for <see cref="IGoogleOfferStore"/>.
    /// Events are raised on the thread that started the call (the Unity main thread).
    /// </summary>
    public sealed class GooglePlayOfferClient
    {
        /// <summary>The message of every call when the offers are not supported here.</summary>
        public const string NotSupportedMessage =
            "Google Play offers are not supported here: they need a Google Play build with Unity IAP 5.4 or later " +
            "on an Android device (not the Editor)";

        internal static readonly TimeSpan QueryTimeout = TimeSpan.FromSeconds(30);

        // Google's BillingResponseCode, Purchase.PurchaseState and ProductDetails.RecurrenceMode values.
        private const int Ok = 0;
        private const int UserCanceled = 1;
        private const int PluginError = -100;
        private const int Purchased = 1;
        private const int Pending = 2;
        private const int FiniteRecurring = 2;
        private const int NonRecurring = 3;

        private readonly IGooglePlayBillingBridge m_Bridge;
        private readonly BillingLog m_Log;

        /// <summary>A purchase was made: verify it (the token and order id are in the order).</summary>
        public event Action<StoreOrder> OrderPending;

        /// <summary>The payment is pending (Google <c>PurchaseState.PENDING</c>).</summary>
        public event Action<StoreOrder> OrderDeferred;

        /// <summary>The purchase failed or the player cancelled it.</summary>
        public event Action<StorePurchaseFailure> PurchaseFailed;

        /// <summary>A replacement with <see cref="ReplacementMode.Deferred"/> was accepted: it takes effect at the next
        /// renewal (the store product id of the new subscription).</summary>
        public event Action<string> ChangeDeferred;

        /// <param name="bridge">The Billing Library calls: <see cref="AndroidGooglePlayBillingBridge"/> in a game.</param>
        /// <param name="log">Where failures are logged.</param>
        public GooglePlayOfferClient(IGooglePlayBillingBridge bridge, BillingLog log = null)
        {
            m_Bridge = bridge;
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
        }

        /// <summary>The bridge can reach the Billing Library (<see cref="IGooglePlayBillingBridge.IsAvailable"/>).</summary>
        public bool IsSupported
        {
            get
            {
                try
                {
                    return m_Bridge != null && m_Bridge.IsAvailable;
                }
                catch (Exception e)
                {
                    m_Log.Warning("Google Play offers unavailable: " + e.Message);
                    return false;
                }
            }
        }

        /// <summary>The base plans and offers of a subscription the player can buy, as Google Play lists them (only
        /// offers the player is eligible for). Throws <see cref="BillingException"/> when they cannot be listed.</summary>
        public async Task<IReadOnlyList<StoreOffer>> GetOffersAsync(string storeProductId, CancellationToken ct)
        {
            if (!IsSupported)
                throw new BillingException(NotSupportedMessage);
            if (string.IsNullOrEmpty(storeProductId))
                throw new BillingException("product id is empty");
            var answer = new TaskCompletionSource<IReadOnlyList<StoreOffer>>();
            var main = SynchronizationContext.Current;
            try
            {
                m_Bridge.QueryOffers(storeProductId, json => MainThreadPost.Run(main, () =>
                {
                    try
                    {
                        answer.TrySetResult(ParseOffers(storeProductId, json));
                    }
                    catch (Exception e)
                    {
                        answer.TrySetException(e);
                    }
                }));
            }
            catch (Exception e)
            {
                throw new BillingException("Google Play offers could not be listed: " + e.Message, e);
            }
            return await TaskTimeout.With(answer.Task, QueryTimeout, ct, "Google Play offers");
        }

        /// <summary>
        /// Starts a purchase of the offer; the result arrives through the events. Fails (never buys another offer) when
        /// the offer is no longer available to the player.
        /// </summary>
        public void Purchase(GooglePlayBillingFlow flow)
        {
            if (flow == null)
                throw new ArgumentNullException(nameof(flow));
            if (!IsSupported)
            {
                Fail(flow.ProductId, NotSupportedMessage);
                return;
            }
            if (string.IsNullOrEmpty(flow.ProductId) || string.IsNullOrEmpty(flow.OfferToken))
            {
                Fail(flow.ProductId, "product id or offer token is empty");
                return;
            }
            var main = SynchronizationContext.Current;
            try
            {
                m_Bridge.Purchase(flow, json => MainThreadPost.Run(main, () => OnPurchaseResult(flow, json)));
            }
            catch (Exception e)
            {
                m_Log.Error($"Google Play offer purchase of {flow.ProductId} not started: {e}");
                Fail(flow.ProductId, "Google Play offer purchase not started: " + e.Message);
            }
        }

        private void OnPurchaseResult(GooglePlayBillingFlow flow, string json)
        {
            int code;
            string message;
            List<GooglePlayPurchase> purchases;
            try
            {
                (code, message, purchases) = ParsePurchases(json, flow.OfferToken);
            }
            catch (Exception e)
            {
                m_Log.Error($"Google Play offer purchase of {flow.ProductId}: unreadable answer: {e.Message}");
                Fail(flow.ProductId, "Google Play offer purchase: unreadable answer from the Android plugin");
                return;
            }
            if (code == UserCanceled)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(flow.ProductId, "UserCancelled: " + message, true));
                return;
            }
            if (code != Ok)
            {
                Fail(flow.ProductId, Describe(code, message));
                return;
            }
            var purchase = purchases.FirstOrDefault(p => p.ProductIds.Contains(flow.ProductId) && !string.IsNullOrEmpty(p.PurchaseToken));
            if (purchase == null)
            {
                if (!string.IsNullOrEmpty(flow.OldProductId) && flow.ReplacementMode == ReplacementMode.Deferred)
                    ChangeDeferred?.Invoke(flow.ProductId);
                else
                    Fail(flow.ProductId, $"Google Play reported no purchase of {flow.ProductId}");
                return;
            }
            var order = new StoreOrder(BillingPlatform.Google, flow.ProductId, purchase.OrderId, purchase.PurchaseToken, purchase);
            if (purchase.IsPending)
                OrderDeferred?.Invoke(order);
            else
                OrderPending?.Invoke(order);
        }

        private void Fail(string productId, string reason)
        {
            m_Log.Warning($"Google Play offer purchase of {productId} failed: {reason}");
            PurchaseFailed?.Invoke(new StorePurchaseFailure(productId, reason, false));
        }

        // ---- JSON of the bridge ----

        /// <summary>The offers of a <see cref="IGooglePlayBillingBridge.QueryOffers"/> answer; throws
        /// <see cref="BillingException"/> for an error answer.</summary>
        internal static IReadOnlyList<StoreOffer> ParseOffers(string productId, string json)
        {
            var root = Parse(json);
            var code = Code(root);
            if (code != Ok)
                throw new BillingException($"Google Play offers of {productId} were not listed: {Describe(code, Str(root, "message"))}");
            var offers = new List<StoreOffer>();
            foreach (var offer in Items(root, "offers").OfType<JObject>())
            {
                var token = Str(offer, "offerToken");
                if (string.IsNullOrEmpty(token))
                    continue;
                var phases = new List<PricingPhase>();
                foreach (var phase in Items(offer, "phases").OfType<JObject>())
                {
                    var micros = (long?)phase["priceMicros"] ?? 0;
                    var mode = (int?)phase["recurrenceMode"] ?? 0;
                    var cycles = (int?)phase["cycles"] ?? 0;
                    // PricingPhase.Cycles: 0 = until cancelled (INFINITE_RECURRING); a prepaid phase is one period.
                    if (mode == NonRecurring)
                        cycles = 1;
                    else if (mode != FiniteRecurring)
                        cycles = 0;
                    phases.Add(new PricingPhase(micros / 1_000_000m, Str(phase, "formattedPrice"), Str(phase, "currency"),
                        Str(phase, "period"), cycles));
                }
                var tags = Items(offer, "tags").Select(t => t.Type == JTokenType.String ? (string)t : null)
                    .Where(t => !string.IsNullOrEmpty(t)).ToList();
                offers.Add(new StoreOffer(Str(offer, "offerId"), token, phases, Str(offer, "basePlanId"), tags));
            }
            return offers;
        }

        /// <summary>The code, message and purchases of a <see cref="IGooglePlayBillingBridge.Purchase"/> answer.</summary>
        internal static (int Code, string Message, List<GooglePlayPurchase> Purchases) ParsePurchases(string json, string offerToken)
        {
            var root = Parse(json);
            var purchases = new List<GooglePlayPurchase>();
            foreach (var p in Items(root, "purchases").OfType<JObject>())
            {
                var state = (int?)p["state"] ?? 0;
                if (state != Purchased && state != Pending)
                    continue;
                var products = Items(p, "products").Select(t => t.Type == JTokenType.String ? (string)t : null)
                    .Where(t => !string.IsNullOrEmpty(t)).ToList();
                purchases.Add(new GooglePlayPurchase(Str(p, "purchaseToken"), Str(p, "orderId"), products, state == Pending,
                    (bool?)p["acknowledged"] ?? false, offerToken));
            }
            return (Code(root), Str(root, "message") ?? "", purchases);
        }

        /// <summary>A failure as the game reads it: <c>ITEM_UNAVAILABLE (4): the offer is not available ...</c>.</summary>
        internal static string Describe(int code, string message)
        {
            string name;
            switch (code)
            {
                case -3: name = "SERVICE_TIMEOUT"; break;
                case -2: name = "FEATURE_NOT_SUPPORTED"; break;
                case -1: name = "SERVICE_DISCONNECTED"; break;
                case 1: name = "USER_CANCELED"; break;
                case 2: name = "SERVICE_UNAVAILABLE"; break;
                case 3: name = "BILLING_UNAVAILABLE"; break;
                case 4: name = "ITEM_UNAVAILABLE"; break;
                case 5: name = "DEVELOPER_ERROR"; break;
                case 6: name = "ERROR"; break;
                case 7: name = "ITEM_ALREADY_OWNED"; break;
                case 8: name = "ITEM_NOT_OWNED"; break;
                case 12: name = "NETWORK_ERROR"; break;
                case PluginError: name = "PLUGIN_ERROR"; break;
                default: name = "CODE"; break;
            }
            var text = $"{name} ({code.ToString(CultureInfo.InvariantCulture)})";
            return string.IsNullOrEmpty(message) ? text : $"{text}: {message}";
        }

        private static JObject Parse(string json)
        {
            try
            {
                return JObject.Parse(json ?? "");
            }
            catch (JsonException e)
            {
                throw new BillingException("unreadable answer from the Google Play plugin: " + e.Message, e);
            }
        }

        private static int Code(JObject root) => root["code"]?.Type == JTokenType.Integer ? (int)root["code"] : PluginError;

        private static string Str(JObject o, string key)
        {
            var token = o[key];
            return token != null && token.Type == JTokenType.String ? (string)token : null;
        }

        private static IEnumerable<JToken> Items(JObject o, string key) =>
            o[key] is JArray array ? (IEnumerable<JToken>)array : new JToken[0];
    }
}
