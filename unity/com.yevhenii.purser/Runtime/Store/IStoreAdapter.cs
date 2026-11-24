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

        /// <summary>Subscriptions: the price for one <paramref name="unit"/> of time (a yearly $49.99 is about $4.17 a
        /// month, $0.96 a week), not rounded; null when <see cref="SubscriptionPeriod"/> is unknown. A month is a twelfth
        /// of a year (30.42 days).</summary>
        public decimal? PricePer(BillingPeriodUnit unit) => BillingPeriods.PricePer(Price, SubscriptionPeriod, unit);

        /// <summary><see cref="PricePer"/> formatted like <see cref="LocalizedPrice"/> (<c>$4.17</c>, <c>4,17 €</c>), for a
        /// paywall's "per month" line; null when the period is unknown.</summary>
        public string LocalizedPricePer(BillingPeriodUnit unit)
        {
            var amount = PricePer(unit);
            return amount.HasValue ? FormatPrice(amount.Value) : null;
        }

        /// <summary>Formats an amount in this product's currency like the store formatted <see cref="LocalizedPrice"/>:
        /// the same currency sign, separators and decimals. For a discount, a total or a price per period.</summary>
        public string FormatPrice(decimal amount) => PriceFormat.Format(LocalizedPrice, Price, amount, Currency);
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

        /// <summary>The phase's price for one <paramref name="unit"/> of time (see <see cref="StoreProduct.PricePer"/>);
        /// null when <see cref="Period"/> is unknown.</summary>
        public decimal? PricePer(BillingPeriodUnit unit) => BillingPeriods.PricePer(Price, Period, unit);

        /// <summary><see cref="PricePer"/> formatted like <see cref="LocalizedPrice"/>; null when the period is unknown.
        /// A free phase is formatted as a zero amount, so check <see cref="IsFreeTrial"/> first.</summary>
        public string LocalizedPricePer(BillingPeriodUnit unit)
        {
            var amount = PricePer(unit);
            return amount.HasValue ? PriceFormat.Format(LocalizedPrice, Price, amount.Value, Currency) : null;
        }

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

    /// <summary>A unit of a subscription period (<see cref="BillingPeriods.TryParse"/>).</summary>
    public enum BillingPeriodUnit
    {
        Day,
        Week,
        Month,
        Year,
    }

    /// <summary>ISO 8601 subscription periods (<c>P1M</c>) as stores and the admin panel write them.</summary>
    public static class BillingPeriods
    {
        /// <summary>Splits a period into its count and unit (<c>P3M</c>: 3, <see cref="BillingPeriodUnit.Month"/>), for
        /// a localized "3 months" label. False when it is not a period.</summary>
        public static bool TryParse(string iso, out int count, out BillingPeriodUnit unit)
        {
            count = 0;
            unit = BillingPeriodUnit.Day;
            var p = (iso ?? "").Trim().ToUpperInvariant();
            if (p.Length < 3 || p[0] != 'P' || !int.TryParse(p.Substring(1, p.Length - 2), out var n) || n <= 0)
                return false;
            switch (p[p.Length - 1])
            {
                case 'D': unit = BillingPeriodUnit.Day; break;
                case 'W': unit = BillingPeriodUnit.Week; break;
                case 'M': unit = BillingPeriodUnit.Month; break;
                case 'Y': unit = BillingPeriodUnit.Year; break;
                default: return false;
            }
            count = n;
            return true;
        }

        /// <summary>A price paid every <paramref name="period"/> converted to one <paramref name="unit"/> of time, not
        /// rounded; null when <paramref name="period"/> is not a period. Years and months convert exactly (12), weeks and
        /// days too (7); between the two a month is a twelfth of 365 days.</summary>
        public static decimal? PricePer(decimal price, string period, BillingPeriodUnit unit)
        {
            if (!TryParse(period, out var count, out var periodUnit))
                return null;
            var dayBased = periodUnit <= BillingPeriodUnit.Week;
            if (dayBased == unit <= BillingPeriodUnit.Week)
                return price * Plain(unit) / (count * Plain(periodUnit));
            return price * Months(unit) / (count * Months(periodUnit));
        }

        /// <summary>One unit in days (day, week) or months (month, year): exact within each pair.</summary>
        private static decimal Plain(BillingPeriodUnit unit) =>
            unit == BillingPeriodUnit.Week ? 7m : unit == BillingPeriodUnit.Year ? 12m : 1m;

        /// <summary>One unit in months (a day is 12 / 365 of a month).</summary>
        private static decimal Months(BillingPeriodUnit unit)
        {
            switch (unit)
            {
                case BillingPeriodUnit.Day: return 12m / 365m;
                case BillingPeriodUnit.Week: return 7m * 12m / 365m;
                case BillingPeriodUnit.Month: return 1m;
                default: return 12m;
            }
        }

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
    /// <c>UnityIapStoreAdapter</c> (Google Play, App Store); <see cref="AmazonStoreAdapter"/>,
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
    /// Implemented by <c>UnityIapStoreAdapter</c>.
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
    /// <c>UnityIapStoreAdapter</c> (Google Play; on the App Store a purchase in the same subscription group
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

    /// <summary>
    /// Optional <see cref="IStoreAdapter"/> capability: the store's own sheet where players redeem offer codes
    /// (<see cref="IBillingService.PresentOfferCodeRedemption"/>). A redeemed code arrives as a purchase through
    /// <see cref="IStoreAdapter.OrderPending"/>. Implemented by <c>UnityIapStoreAdapter</c> on the App Store.
    /// </summary>
    public interface IOfferCodeRedemptionStore
    {
        /// <summary>Shows the sheet; false when the store cannot show it.</summary>
        bool PresentOfferCodeRedemption();
    }

    /// <summary>
    /// Optional <see cref="IStoreAdapter"/> capability: the store's page where a player manages subscriptions comes
    /// from the billing server or the adapter's settings (<see cref="IBillingService.OpenManageSubscriptionsAsync"/>).
    /// Implemented by <see cref="StripeStoreAdapter"/>.
    /// </summary>
    public interface ISubscriptionManagementStore
    {
        /// <summary>The page for <paramref name="userId"/> (null when not set), or null when there is none.</summary>
        Task<string> GetManageSubscriptionsUrlAsync(string userId, CancellationToken ct);
    }
}
