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

        public StoreProduct(string id, string storeId, BillingProductType type, string localizedPrice, decimal price,
            string currency, string title, string description, bool availableToPurchase, object native = null)
        {
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

    /// <summary>A store transaction waiting to be verified / confirmed.</summary>
    public sealed class StoreOrder
    {
        public BillingPlatform Platform { get; }
        public string StoreProductId { get; }
        /// <summary>Apple transactionId / Amazon receiptId / Google orderId (may be empty for Google test purchases).</summary>
        public string TransactionId { get; }
        /// <summary>Google purchase token; empty on Apple.</summary>
        public string PurchaseToken { get; }
        public string OriginalTransactionId { get; }
        /// <summary>Google obfuscatedAccountId / Apple appAccountToken the purchase was made with.</summary>
        public string AccountId { get; }
        /// <summary>Underlying store object (Unity IAP <c>PendingOrder</c>/<c>Order</c>, <see cref="AmazonReceipt"/>).</summary>
        public object Native { get; }
        /// <summary>The player's id in the store when the server needs it to look the purchase up (Amazon userId).</summary>
        public string StoreUserId { get; }

        public StoreOrder(BillingPlatform platform, string storeProductId, string transactionId, string purchaseToken,
            string originalTransactionId = null, string accountId = null, object native = null, string storeUserId = null)
        {
            Platform = platform;
            StoreProductId = storeProductId;
            TransactionId = transactionId;
            PurchaseToken = purchaseToken;
            OriginalTransactionId = originalTransactionId;
            AccountId = accountId;
            Native = native;
            StoreUserId = storeUserId;
        }

        /// <summary>Stable key of the purchase in the store (Google token / Apple transaction id).</summary>
        public string Key => Platform == BillingPlatform.Google && !string.IsNullOrEmpty(PurchaseToken) ? PurchaseToken : TransactionId;

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
    /// <see cref="UnityIapStoreAdapter"/> (Google Play, App Store); <see cref="AmazonStoreAdapter"/> serves the
    /// Amazon Appstore. Steam and web stores are outside Unity IAP: implement this interface (and
    /// <see cref="IStoreNameProvider"/> when the platform does not imply the store) and pass it in
    /// <see cref="BillingDependencies.Store"/>. Events must be raised on the Unity main thread.
    /// </summary>
    public interface IStoreAdapter : IDisposable
    {
        BillingPlatform Platform { get; }

        /// <summary>Paid purchase waiting for verification (new, re-delivered after restart, or restored).</summary>
        event Action<StoreOrder> OrderPending;

        /// <summary>Purchase waiting for an external approval (Ask to Buy, pending Google payment).</summary>
        event Action<StoreOrder> OrderDeferred;

        event Action<StorePurchaseFailure> PurchaseFailed;

        /// <summary>Result of <see cref="Confirm"/>: (order, success, error).</summary>
        event Action<StoreOrder, bool, string> OrderConfirmed;

        Task ConnectAsync(CancellationToken ct);

        Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct);

        /// <summary>Binds future purchases to a player (Google obfuscatedAccountId, Apple appAccountToken).</summary>
        void SetAccountBinding(string googleAccountId, Guid? appleAccountToken);

        void Purchase(string storeProductId);

        /// <summary>Finishes the transaction in the store (after the server accepted or rejected it).</summary>
        void Confirm(StoreOrder order);

        /// <summary>Purchases the store currently reports (non-consumables, subscriptions, unfinished orders).</summary>
        Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct);

        /// <summary>Apple "Restore purchases": re-delivers owned purchases through <see cref="OrderPending"/>.</summary>
        Task<bool> RestoreTransactionsAsync(CancellationToken ct);
    }
}
