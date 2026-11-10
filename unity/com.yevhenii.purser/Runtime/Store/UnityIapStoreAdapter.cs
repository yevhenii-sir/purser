using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using UnityEngine.Purchasing;

namespace Purser
{
    /// <summary>
    /// <see cref="IStoreAdapter"/> on Unity IAP 5 (<c>StoreController</c>). Unity IAP products are registered
    /// with the store product id as their id, so orders map 1:1 to the billing server catalog.
    /// </summary>
    public sealed partial class UnityIapStoreAdapter : IStoreAdapter
    {
        private static readonly TimeSpan OperationTimeout = TimeSpan.FromSeconds(30);

        private readonly StoreController m_Store;
        private readonly BillingLog m_Log;
        private readonly Dictionary<string, BillingProductType> m_Types = new Dictionary<string, BillingProductType>();

        private TaskCompletionSource<bool> m_Connect;
        private TaskCompletionSource<IReadOnlyList<StoreProduct>> m_Products;
        private IReadOnlyList<BillingProductConfig> m_RequestedProducts;
        private TaskCompletionSource<IReadOnlyList<StoreOrder>> m_Purchases;

        public BillingPlatform Platform { get; }

        /// <summary><c>google</c> or <c>apple</c>.</summary>
        public string Store { get; }

        public event Action<StoreOrder> OrderPending;
        public event Action<StoreOrder> OrderDeferred;
        public event Action<StorePurchaseFailure> PurchaseFailed;
        public event Action<StoreOrder, bool, string> OrderFinished;

        /// <param name="platform"><see cref="BillingPlatform.Google"/> or <see cref="BillingPlatform.Apple"/>; other
        /// platforms run Unity IAP's fake store (Editor) under the Google store id.</param>
        public UnityIapStoreAdapter(BillingPlatform platform, BillingLog log = null)
        {
            Platform = platform;
            Store = platform == BillingPlatform.Apple ? BillingStores.Apple : BillingStores.Google;
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
            m_Store = UnityIAPServices.StoreController();

            // Unity IAP requires every event (success and failure) to be subscribed before Connect():
            // unfinished purchases from a previous session are delivered right after connecting.
            m_Store.OnStoreConnected += HandleConnected;
            m_Store.OnStoreDisconnected += HandleDisconnected;
            m_Store.OnProductsFetched += HandleProductsFetched;
            m_Store.OnProductsFetchFailed += HandleProductsFetchFailed;
            m_Store.OnPurchasesFetched += HandlePurchasesFetched;
            m_Store.OnPurchasesFetchFailed += HandlePurchasesFetchFailed;
            m_Store.OnPurchasePending += HandlePurchasePending;
            m_Store.OnPurchaseConfirmed += HandlePurchaseConfirmed;
            m_Store.OnPurchaseFailed += HandlePurchaseFailed;
            m_Store.OnPurchaseDeferred += HandlePurchaseDeferred;
        }

        public async Task ConnectAsync(CancellationToken ct)
        {
            m_Connect = new TaskCompletionSource<bool>();
            var connect = m_Store.Connect();
            await Task.WhenAny(connect, m_Connect.Task);
            if (connect.IsFaulted && !m_Connect.Task.IsCompleted)
                throw new BillingException("Store connection failed: " + connect.Exception?.GetBaseException().Message);
            if (m_Store.GetConnectionState() == ConnectionState.Connected)
                return;
            await TaskTimeout.With(m_Connect.Task, OperationTimeout, ct, "store connection");
        }

        public async Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            m_RequestedProducts = products;
            m_Types.Clear();
            var definitions = new List<ProductDefinition>();
            foreach (var p in products)
            {
                var storeId = p.StoreId(Store);
                if (m_Types.ContainsKey(storeId))
                    continue;
                m_Types[storeId] = p.type;
                definitions.Add(new ProductDefinition(storeId, storeId, ToUnityType(p.type)));
            }
            m_Products = new TaskCompletionSource<IReadOnlyList<StoreProduct>>();
            m_Store.FetchProducts(definitions);
            return await TaskTimeout.With(m_Products.Task, OperationTimeout, ct, "product fetch");
        }

        /// <summary>Google <c>obfuscatedAccountId</c> / Apple <c>appAccountToken</c> derived from the user id
        /// (<see cref="AccountIds"/>, the same as the server).</summary>
        public void SetAccountBinding(string userId)
        {
            if (string.IsNullOrEmpty(userId))
                return;
            if (Platform == BillingPlatform.Apple)
                m_Store.AppleStoreExtendedService?.SetAppAccountToken(AccountIds.AppleAccountToken(userId));
            else
                m_Store.GooglePlayStoreExtendedService?.SetObfuscatedAccountId(AccountIds.GoogleAccountId(userId));
        }

        public void Purchase(string storeProductId)
        {
            var product = m_Store.GetProductById(storeProductId);
            if (product == null || !product.availableToPurchase)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "product is not available in the store", false));
                return;
            }
            m_Store.PurchaseProduct(product);
        }

        /// <summary>Google consume / acknowledge (the server already did it for server deliveries), Apple
        /// finishTransaction. Unity IAP finishes rejected purchases the same way.</summary>
        public void Finish(StoreOrder order, bool granted)
        {
            if (order.Native is PendingOrder pending)
                m_Store.ConfirmPurchase(pending);
        }

        public async Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct)
        {
            m_Purchases = new TaskCompletionSource<IReadOnlyList<StoreOrder>>();
            m_Store.FetchPurchases();
            return await TaskTimeout.With(m_Purchases.Task, OperationTimeout, ct, "purchase fetch");
        }

        public async Task<bool> RestoreTransactionsAsync(CancellationToken ct)
        {
            var tcs = new TaskCompletionSource<bool>();
            m_Store.RestoreTransactions((success, error) =>
            {
                if (!success)
                    m_Log.Warning("Restore transactions failed: " + error);
                tcs.TrySetResult(success);
            });
            return await TaskTimeout.With(tcs.Task, OperationTimeout, ct, "restore");
        }

        public void Dispose()
        {
            m_Store.OnStoreConnected -= HandleConnected;
            m_Store.OnStoreDisconnected -= HandleDisconnected;
            m_Store.OnProductsFetched -= HandleProductsFetched;
            m_Store.OnProductsFetchFailed -= HandleProductsFetchFailed;
            m_Store.OnPurchasesFetched -= HandlePurchasesFetched;
            m_Store.OnPurchasesFetchFailed -= HandlePurchasesFetchFailed;
            m_Store.OnPurchasePending -= HandlePurchasePending;
            m_Store.OnPurchaseConfirmed -= HandlePurchaseConfirmed;
            m_Store.OnPurchaseFailed -= HandlePurchaseFailed;
            m_Store.OnPurchaseDeferred -= HandlePurchaseDeferred;
        }

        // ---- Unity IAP events ----

        private void HandleConnected() => m_Connect?.TrySetResult(true);

        private void HandleDisconnected(StoreConnectionFailureDescription failure)
        {
            var message = failure?.Message ?? "unknown";
            m_Log.Warning("Store disconnected: " + message);
            m_Connect?.TrySetException(new BillingException("Store connection failed: " + message));
        }

        private void HandleProductsFetched(List<Product> products)
        {
            var result = new List<StoreProduct>();
            foreach (var product in products)
            {
                var storeId = product.definition?.storeSpecificId ?? product.definition?.id;
                var config = FindConfig(storeId);
                var meta = product.metadata;
                result.Add(new StoreProduct(
                    config?.id ?? storeId,
                    storeId,
                    config?.type ?? FromUnityType(product.definition?.type ?? ProductType.Consumable),
                    meta?.localizedPriceString,
                    meta?.localizedPrice ?? 0m,
                    meta?.isoCurrencyCode,
                    meta?.localizedTitle,
                    meta?.localizedDescription,
                    product.availableToPurchase,
                    product));
            }
            m_Products?.TrySetResult(result);
        }

        private void HandleProductsFetchFailed(ProductFetchFailed failure)
        {
            var failed = failure?.FailedFetchProducts?.Select(d => d.id).ToList() ?? new List<string>();
            m_Log.Warning($"Products not fetched ({failure?.FailureReason}): {string.Join(", ", failed)}");
            // Unity IAP reports the products that did load through OnProductsFetched; fail only if nothing loaded.
            if (failed.Count >= m_Types.Count)
                m_Products?.TrySetException(new BillingException("Product fetch failed: " + failure?.FailureReason));
        }

        private void HandlePurchasesFetched(Orders orders)
        {
            var result = new List<StoreOrder>();
            foreach (var o in orders.ConfirmedOrders)
                AddIfMapped(result, o);
            foreach (var o in orders.PendingOrders)
                AddIfMapped(result, o);
            m_Purchases?.TrySetResult(result);
        }

        private void HandlePurchasesFetchFailed(PurchasesFetchFailureDescription failure)
        {
            m_Purchases?.TrySetException(new BillingException($"Purchase fetch failed ({failure?.FailureReason}): {failure?.Message}"));
        }

        private void HandlePurchasePending(PendingOrder order)
        {
            var mapped = Map(order);
            if (mapped == null)
            {
                m_Log.Error("Pending order without product or transaction id; it will be retried by the store.");
                return;
            }
            OrderPending?.Invoke(mapped);
        }

        private void HandlePurchaseConfirmed(Order order)
        {
            var mapped = Map(order);
            if (order is FailedOrder failed)
                OrderFinished?.Invoke(mapped, false, $"{failed.FailureReason}: {failed.Details}");
            else
                OrderFinished?.Invoke(mapped, true, null);
        }

        private void HandlePurchaseFailed(FailedOrder order)
        {
            var storeId = ProductOf(order)?.definition?.storeSpecificId;
            var cancelled = order.FailureReason == PurchaseFailureReason.UserCancelled;
            PurchaseFailed?.Invoke(new StorePurchaseFailure(storeId, $"{order.FailureReason}: {order.Details}", cancelled));
        }

        private void HandlePurchaseDeferred(DeferredOrder order)
        {
            var mapped = Map(order);
            if (mapped != null)
                OrderDeferred?.Invoke(mapped);
        }

        // ---- mapping ----

        private void AddIfMapped(List<StoreOrder> list, Order order)
        {
            var mapped = Map(order);
            if (mapped != null)
                list.Add(mapped);
        }

        private StoreOrder Map(Order order)
        {
            if (order == null)
                return null;
            var storeId = ProductOf(order)?.definition?.storeSpecificId ?? ProductOf(order)?.definition?.id;
            var info = order.Info;
            if (storeId == null || info == null)
                return null;

            if (Platform == BillingPlatform.Google)
            {
                var token = info.Google?.PurchaseToken;
                if (string.IsNullOrEmpty(token))
                    token = info.TransactionID;
                if (string.IsNullOrEmpty(token))
                    ReceiptParser.TryGetGooglePurchaseToken(info.Receipt, out token);
                var orderId = info.Google?.OrderId;
                if (string.IsNullOrEmpty(orderId))
                    ReceiptParser.TryGetGoogleOrderId(info.Receipt, out orderId);
                return string.IsNullOrEmpty(token)
                    ? null
                    : new StoreOrder(Platform, storeId, orderId, token, order);
            }

            var transactionId = info.TransactionID;
            if (string.IsNullOrEmpty(transactionId))
                ReceiptParser.TryGetTransactionId(info.Receipt, out transactionId);
            return string.IsNullOrEmpty(transactionId)
                ? null
                : new StoreOrder(Platform, storeId, transactionId, null, order);
        }

        private static Product ProductOf(Order order) => order?.CartOrdered?.Items()?.FirstOrDefault()?.Product;

        private BillingProductConfig FindConfig(string storeId)
        {
            if (m_RequestedProducts == null)
                return null;
            foreach (var p in m_RequestedProducts)
                if (p.StoreId(Store) == storeId)
                    return p;
            return null;
        }

        private static ProductType ToUnityType(BillingProductType type)
        {
            switch (type)
            {
                case BillingProductType.NonConsumable: return ProductType.NonConsumable;
                case BillingProductType.Subscription: return ProductType.Subscription;
                default: return ProductType.Consumable;
            }
        }

        private static BillingProductType FromUnityType(ProductType type)
        {
            switch (type)
            {
                case ProductType.NonConsumable: return BillingProductType.NonConsumable;
                case ProductType.Subscription: return BillingProductType.Subscription;
                default: return BillingProductType.Consumable;
            }
        }
    }
}
