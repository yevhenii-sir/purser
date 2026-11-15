using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>Product as loaded from the store (localized price etc.).</summary>
    public sealed class StoreProduct
    {
        /// <summary>Id from <see cref="BillingConfig"/>.</summary>
        public string Id { get; }
        public string StoreId { get; }
        public BillingProductType Type { get; }
        public string LocalizedPrice { get; }
        public decimal Price { get; }
        /// <summary>ISO 4217 code, e.g. USD.</summary>
        public string Currency { get; }
        public string Title { get; }
        public string Description { get; }
        public bool AvailableToPurchase { get; }
        /// <summary>Underlying store object (Unity IAP <c>Product</c>).</summary>
        public object Native { get; }
        /// <summary>The price is the config's <see cref="BillingProductConfig.fallbackPrice"/>: the store gave none. It is
        /// for display only and is not reported to the server.</summary>
        public bool IsFallbackPrice { get; }

        /// <summary>Subscriptions: the billing period as ISO 8601 (<c>P1W</c>, <c>P1M</c>, <c>P3M</c>, <c>P6M</c>,
        /// <c>P1Y</c>), empty when unknown. Google Play reports it; elsewhere (the App Store through Unity IAP, Steam, web
        /// stores, the Editor) it is the config's <see cref="BillingProductConfig.fallbackPeriod"/>.</summary>
        public string SubscriptionPeriod { get; }

        /// <summary><see cref="SubscriptionPeriod"/> is the config's fallback period: the store gave none.</summary>
        public bool IsFallbackPeriod { get; }

        /// <summary><see cref="SubscriptionPeriod"/> in days (a month is 30, a year 365), 0 when unknown: to compare
        /// the price per day of a weekly and a yearly subscription.</summary>
        public int SubscriptionPeriodDays => BillingPeriods.Days(SubscriptionPeriod);

        /// <summary>
        /// Subscriptions: the offers the store reports, with their pricing phases (free trial, introductory price, the
        /// regular price). Google Play through Unity IAP 5: the one offer Unity IAP buys (the first offer of the
        /// product), without its id and token, which Unity IAP keeps internal. App Store: the introductory offer
        /// (whether the player can still get it is <see cref="IntroEligible"/>). Empty elsewhere.
        /// </summary>
        public IReadOnlyList<StoreOffer> Offers { get; }

        /// <summary>Whether the player can get the introductory offer (free trial or intro price); null when the store
        /// does not say. App Store: StoreKit's <c>isEligibleForIntroOffer</c>. Google Play lists only offers the
        /// player is eligible for: true when the offer has a trial or intro phase, otherwise unknown.</summary>
        public bool? IntroEligible { get; }

        public StoreProduct(string id, string storeId, BillingProductType type, string localizedPrice, decimal price,
            string currency, string title, string description, bool availableToPurchase, object native = null,
            bool isFallbackPrice = false, string subscriptionPeriod = null, bool isFallbackPeriod = false,
            IReadOnlyList<StoreOffer> offers = null, bool? introEligible = null)
        {
            Offers = offers ?? new StoreOffer[0];
            IntroEligible = introEligible;
            IsFallbackPrice = isFallbackPrice;
            SubscriptionPeriod = BillingPeriods.Normalize(subscriptionPeriod);
            IsFallbackPeriod = isFallbackPeriod && SubscriptionPeriod.Length > 0;
            Id = id;
            StoreId = storeId;
            Type = type;
            LocalizedPrice = localizedPrice;
            Price = price;
            Currency = currency;
            Title = title;
            Description = description;
            AvailableToPurchase = availableToPurchase;
            Native = native;
        }

        public long PriceMicros => (long)decimal.Round(Price * 1_000_000m);
    }

    /// <summary>A subscription offer of a store product (<see cref="StoreProduct.Offers"/>).</summary>
    public sealed class StoreOffer
    {
        /// <summary>The store's offer id (App Store offer id, Google offer id); null when the store does not give it.</summary>
        public string Id { get; }

        /// <summary>Google Play offer token; null when the store or Unity IAP does not give it (Unity IAP 5 keeps it
        /// internal).</summary>
        public string Token { get; }

        /// <summary>The phases in order: free trial, introductory price, then the regular price.</summary>
        public IReadOnlyList<PricingPhase> PricingPhases { get; }

        /// <summary>The offer starts with a free trial.</summary>
        public bool HasFreeTrial
        {
            get
            {
                foreach (var phase in PricingPhases)
                    if (phase.IsFreeTrial)
                        return true;
                return false;
            }
        }

        /// <summary>Creates an offer (store adapters, tests).</summary>
        public StoreOffer(string id, string token, IReadOnlyList<PricingPhase> pricingPhases)
        {
            Id = string.IsNullOrEmpty(id) ? null : id;
            Token = string.IsNullOrEmpty(token) ? null : token;
            PricingPhases = pricingPhases ?? new PricingPhase[0];
        }
    }

    /// <summary>One phase of a <see cref="StoreOffer"/>: a price for a number of billing periods.</summary>
    public sealed class PricingPhase
    {
        /// <summary>Price per billing period (0 for a free trial).</summary>
        public decimal Price { get; }

        /// <summary>The price as the store formats it, e.g. <c>$0.99</c>; empty when unknown.</summary>
        public string LocalizedPrice { get; }

        /// <summary>ISO 4217 currency, e.g. USD; null when unknown.</summary>
        public string Currency { get; }

        /// <summary>Billing period, ISO 8601 (<c>P1W</c>, <c>P1M</c>, <c>P1Y</c>).</summary>
        public string Period { get; }

        /// <summary>How many periods the phase lasts; 0 = it recurs until cancelled (the regular price).</summary>
        public int Cycles { get; }

        /// <summary>The phase is free.</summary>
        public bool IsFreeTrial => Price == 0m;

        /// <summary>Creates a phase (store adapters, tests).</summary>
        public PricingPhase(decimal price, string localizedPrice, string currency, string period, int cycles)
        {
            Price = price < 0 ? 0 : price;
            LocalizedPrice = localizedPrice ?? "";
            Currency = string.IsNullOrEmpty(currency) ? null : currency;
            Period = BillingPeriods.Normalize(period);
            Cycles = Math.Max(0, cycles);
        }
    }

    /// <summary>ISO 8601 subscription periods (<c>P1M</c>) as stores and the admin panel write them.</summary>
    public static class BillingPeriods
    {
        /// <summary>The trimmed, upper-case period when it looks like <c>P&lt;n&gt;D|W|M|Y</c>; empty otherwise.</summary>
        public static string Normalize(string iso)
        {
            var p = (iso ?? "").Trim().ToUpperInvariant();
            return Days(p) > 0 ? p : "";
        }

        /// <summary>Approximate days of a period (<c>P1W</c> 7, <c>P1M</c> 30, <c>P1Y</c> 365), 0 when not a period.</summary>
        public static int Days(string iso)
        {
            var p = (iso ?? "").Trim().ToUpperInvariant();
            if (p.Length < 3 || p[0] != 'P' || !int.TryParse(p.Substring(1, p.Length - 2), out var n) || n <= 0)
                return 0;
            switch (p[p.Length - 1])
            {
                case 'D': return n;
                case 'W': return n * 7;
                case 'M': return n * 30;
                case 'Y': return n * 365;
                default: return 0;
            }
        }
    }

    /// <summary>A store transaction waiting to be verified, then finished in the store.</summary>
    public sealed class StoreOrder
    {
        public BillingPlatform Platform { get; }
        /// <summary>Product id in the store; empty when the store does not say (the server's answer names it).</summary>
        public string StoreProductId { get; }
        /// <summary>Apple transactionId / Amazon receiptId / Google orderId (may be empty for Google test purchases) /
        /// Steam, Stripe or Xsolla order id.</summary>
        public string TransactionId { get; }
        /// <summary>Google purchase token; empty elsewhere.</summary>
        public string PurchaseToken { get; }
        /// <summary>Underlying store object (Unity IAP <c>PendingOrder</c>/<c>Order</c>, <see cref="AmazonReceipt"/>).</summary>
        public object Native { get; }
        /// <summary>The player's id in the store when the server needs it to look the purchase up (Amazon userId,
        /// SteamID); for server checkouts, the billing user the order was started for.</summary>
        public string StoreUserId { get; }

        public StoreOrder(BillingPlatform platform, string storeProductId, string transactionId, string purchaseToken,
            object native = null, string storeUserId = null)
        {
            Platform = platform;
            StoreProductId = storeProductId ?? "";
            TransactionId = transactionId;
            PurchaseToken = purchaseToken;
            Native = native;
            StoreUserId = storeUserId;
        }

        /// <summary>Stable key of the purchase in the store (Google token, otherwise the transaction id).</summary>
        public string Key => !string.IsNullOrEmpty(PurchaseToken) ? PurchaseToken : TransactionId;

        public override string ToString() => $"{Platform} {StoreProductId} {TransactionId}";
    }

    public sealed class StorePurchaseFailure
    {
        public string StoreProductId { get; }
        public string Reason { get; }
        public bool UserCancelled { get; }

        public StorePurchaseFailure(string storeProductId, string reason, bool userCancelled)
        {
            StoreProductId = storeProductId;
            Reason = reason;
            UserCancelled = userCancelled;
        }
    }

    /// <summary>
    /// Store abstraction used by <see cref="BillingService"/>. The default implementation is
    /// <see cref="UnityIapStoreAdapter"/> (Google Play, App Store); <see cref="AmazonStoreAdapter"/>,
    /// <see cref="SteamStoreAdapter"/>, <see cref="StripeStoreAdapter"/> and <see cref="XsollaStoreAdapter"/> serve the
    /// other stores. Pass one in <see cref="BillingDependencies.Store"/>. Another store whose purchase starts on the
    /// billing server derives from <see cref="ServerCheckoutStoreAdapter"/>. Events must be raised on the Unity main
    /// thread.
    /// </summary>
    public interface IStoreAdapter : IDisposable
    {
        /// <summary>Device platform the store runs on (Apple Ads, account binding, the Unity IAP fake store).</summary>
        BillingPlatform Platform { get; }

        /// <summary>Store id of the billing server (<see cref="BillingStores"/>): the <c>/verify/{store}</c> route, the
        /// build in <see cref="BillingConfig.builds"/> and the product ids in <see cref="BillingProductConfig.storeIds"/>.</summary>
        string Store { get; }

        /// <summary>Paid purchase waiting for verification (new, re-delivered after restart, or restored).</summary>
        event Action<StoreOrder> OrderPending;

        /// <summary>Purchase waiting for an external approval (Ask to Buy, pending Google payment).</summary>
        event Action<StoreOrder> OrderDeferred;

        /// <summary>The store refused or the player cancelled a purchase.</summary>
        event Action<StorePurchaseFailure> PurchaseFailed;

        /// <summary>Result of <see cref="Finish"/>: (order, success, error).</summary>
        event Action<StoreOrder, bool, string> OrderFinished;

        /// <summary>Connects to the store; unfinished purchases of earlier sessions arrive through <see cref="OrderPending"/>.</summary>
        Task ConnectAsync(CancellationToken ct);

        /// <summary>Loads the configured products from the store (localized prices where the store has them).</summary>
        Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct);

        /// <summary>Binds future purchases to a player, when the store supports it (Google obfuscatedAccountId, Apple
        /// appAccountToken, both derived with <see cref="AccountIds"/>).</summary>
        void SetAccountBinding(string userId);

        /// <summary>Starts a purchase; the result arrives through the events.</summary>
        void Purchase(string storeProductId);

        /// <summary>Finishes the transaction in the store after the server's definitive answer: <paramref name="granted"/>
        /// when it accepted the purchase, false when it rejected it (Amazon then reports it as unavailable).</summary>
        void Finish(StoreOrder order, bool granted);

        /// <summary>Purchases the store currently reports (non-consumables, subscriptions, unfinished orders).</summary>
        Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct);

        /// <summary>Apple "Restore purchases": re-delivers owned purchases through <see cref="OrderPending"/>.</summary>
        Task<bool> RestoreTransactionsAsync(CancellationToken ct);
    }

    /// <summary>
    /// Optional <see cref="IStoreAdapter"/> capability: the store connection can drop after
    /// <see cref="IStoreAdapter.ConnectAsync"/> succeeded. <see cref="BillingService"/> then reconnects with
    /// <see cref="IStoreAdapter.ConnectAsync"/> (with backoff) and raises <see cref="IBillingService.StoreDisconnected"/>.
    /// Implemented by <see cref="UnityIapStoreAdapter"/>.
    /// </summary>
    public interface IStoreConnectionEvents
    {
        /// <summary>The connected store disconnected (the reason, for logs). Not raised while connecting: a failed
        /// <see cref="IStoreAdapter.ConnectAsync"/> throws instead.</summary>
        event Action<string> Disconnected;
    }

    /// <summary>
    /// Optional <see cref="IStoreAdapter"/> capability: a subscription purchase that replaces the player's current
    /// subscription (upgrade, downgrade). The result arrives like any purchase. Implemented by
    /// <see cref="UnityIapStoreAdapter"/> (Google Play; on the App Store a purchase in the same subscription group
    /// replaces the old one by itself).
    /// </summary>
    public interface ISubscriptionChangeStore
    {
        /// <summary>Buys <paramref name="storeProductId"/> in place of the owned <paramref name="replacedStoreProductId"/>.</summary>
        void PurchaseReplacing(string storeProductId, string replacedStoreProductId, ReplacementMode mode);

        /// <summary>A change with <see cref="ReplacementMode.Deferred"/> was accepted: it takes effect at the next
        /// renewal and has no transaction now (the store product id of the new subscription).</summary>
        event Action<string> ChangeDeferred;
    }
}
