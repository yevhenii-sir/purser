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

        public StoreProduct(string id, string storeId, BillingProductType type, string localizedPrice, decimal price,
            string currency, string title, string description, bool availableToPurchase, object native = null,
            bool isFallbackPrice = false)
        {
            IsFallbackPrice = isFallbackPrice;
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
}
