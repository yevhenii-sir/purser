using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>HMS IAP <c>PriceType</c> of a product.</summary>
    public enum HuaweiPriceType
    {
        /// <summary><c>IN_APP_CONSUMABLE</c> (0).</summary>
        Consumable = 0,
        /// <summary><c>IN_APP_NONCONSUMABLE</c> (1).</summary>
        NonConsumable = 1,
        /// <summary><c>IN_APP_SUBSCRIPTION</c> (2).</summary>
        Subscription = 2,
    }

    /// <summary>Return codes of an HMS IAP purchase (<c>OrderStatusCode</c>).</summary>
    public static class HuaweiStatus
    {
        /// <summary><c>ORDER_STATE_SUCCESS</c>.</summary>
        public const int Success = 0;
        /// <summary><c>ORDER_STATE_CANCEL</c>: the player closed the payment screen.</summary>
        public const int Cancelled = 60000;
        /// <summary><c>ORDER_PRODUCT_OWNED</c>: the player owns it (a consumable not consumed yet, a non-consumable).</summary>
        public const int ProductOwned = 60051;
    }

    /// <summary>An HMS IAP <c>ProductInfo</c>.</summary>
    public sealed class HuaweiProduct
    {
        public string ProductId;
        public HuaweiPriceType PriceType;
        /// <summary>The price as HMS formats it, e.g. <c>€1.99</c>.</summary>
        public string Price;
        /// <summary><c>microsPrice</c>: the price × 1,000,000.</summary>
        public long MicrosPrice;
        /// <summary>ISO 4217 currency.</summary>
        public string Currency;
        public string ProductName;
        public string ProductDesc;
        /// <summary>Subscriptions: <c>subPeriod</c>, ISO 8601 (<c>P1M</c>).</summary>
        public string SubPeriod;
    }

    /// <summary>An HMS IAP <c>InAppPurchaseData</c> (the fields the billing server needs).</summary>
    public sealed class HuaweiPurchase
    {
        public string OrderId;
        public string PurchaseToken;
        public string ProductId;
        public HuaweiPriceType PriceType;
        /// <summary>Subscriptions: <c>subscriptionId</c>, the same for every renewal.</summary>
        public string SubscriptionId;
        /// <summary>0 purchased, 1 cancelled, 2 refunded.</summary>
        public int PurchaseState;
    }

    /// <summary>The outcome of <see cref="IHuaweiIap.PurchaseAsync"/>.</summary>
    public sealed class HuaweiPurchaseResult
    {
        /// <summary><see cref="HuaweiStatus"/> or another HMS code.</summary>
        public int ReturnCode;
        public string Message;
        /// <summary>The purchase when <see cref="ReturnCode"/> is <see cref="HuaweiStatus.Success"/>.</summary>
        public HuaweiPurchase Purchase;
    }

    /// <summary>
    /// The HMS In-App Purchases calls the billing package needs. Unity IAP has no Huawei store, so the game provides
    /// this bridge over Huawei's IAP SDK (e.g. the HMS Unity Plugin; a ready-made bridge is in the "Huawei AppGallery"
    /// sample). Tasks complete on the Unity main thread. A failed call throws.
    /// </summary>
    public interface IHuaweiIap
    {
        /// <summary><c>isEnvReady</c>: an HMS account is signed in and IAP is available in its country.</summary>
        Task InitializeAsync(CancellationToken ct);
        /// <summary><c>obtainProductInfo</c> for products of one price type.</summary>
        Task<IReadOnlyList<HuaweiProduct>> GetProductsAsync(HuaweiPriceType type, IReadOnlyList<string> productIds, CancellationToken ct);
        /// <summary><c>createPurchaseIntent</c> and the payment screen; never consumes the purchase.</summary>
        Task<HuaweiPurchaseResult> PurchaseAsync(string productId, HuaweiPriceType type, string developerPayload, CancellationToken ct);
        /// <summary><c>obtainOwnedPurchases</c>: consumables bought and not consumed, non-consumables, active
        /// subscriptions.</summary>
        Task<IReadOnlyList<HuaweiPurchase>> GetOwnedPurchasesAsync(HuaweiPriceType type, CancellationToken ct);
        /// <summary><c>consumeOwnedPurchase</c>.</summary>
        Task ConsumeAsync(string purchaseToken, CancellationToken ct);
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for Huawei AppGallery (HMS devices). The billing server looks purchases up in
    /// Huawei's IAP services (<c>/verify/huawei</c>: the purchase token, and for subscriptions the subscription id);
    /// after its answer the adapter consumes consumables. Pass it in <see cref="BillingDependencies.Store"/>:
    /// <code>new BillingDependencies { Store = new HuaweiStoreAdapter(new HuaweiIapBridge()) }</code>
    /// </summary>
    public sealed class HuaweiStoreAdapter : IStoreAdapter
    {
        private static readonly TimeSpan OperationTimeout = TimeSpan.FromSeconds(30);
        /// <summary>A purchase whose payment screen never answered (an intent that failed without a callback) fails
        /// after this long instead of waiting forever.</summary>
        private static readonly TimeSpan PurchaseTimeout = TimeSpan.FromMinutes(15);
        private static readonly HuaweiPriceType[] AllTypes = { HuaweiPriceType.Consumable, HuaweiPriceType.NonConsumable, HuaweiPriceType.Subscription };

        private readonly IHuaweiIap m_Iap;
        private readonly BillingLog m_Log;
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        private IReadOnlyList<BillingProductConfig> m_Products = new BillingProductConfig[0];

        /// <inheritdoc/>
        public BillingPlatform Platform => BillingPlatform.Huawei;
        /// <inheritdoc/>
        public string Store => BillingStores.Huawei;

        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <summary>HMS reports pending payments as failures; nothing is deferred.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<StoreOrder, bool, string> OrderFinished;

        /// <param name="iap">Bridge to the HMS IAP SDK.</param>
        public HuaweiStoreAdapter(IHuaweiIap iap, BillingLog log = null)
        {
            m_Iap = iap ?? throw new ArgumentNullException(nameof(iap));
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
        }

        /// <summary>Checks that IAP is available, then raises the consumables bought and not consumed yet (a purchase
        /// whose answer never came) through <see cref="OrderPending"/>.</summary>
        public async Task ConnectAsync(CancellationToken ct)
        {
            await TaskTimeout.With(Done(m_Iap.InitializeAsync(ct)), OperationTimeout, ct, "HMS IAP");
            IReadOnlyList<HuaweiPurchase> unconsumed;
            try
            {
                unconsumed = await TaskTimeout.With(m_Iap.GetOwnedPurchasesAsync(HuaweiPriceType.Consumable, ct), OperationTimeout, ct, "HMS owned purchases");
            }
            catch (BillingException e)
            {
                m_Log.Warning("HMS owned consumables at startup: " + e.Message);
                return;
            }
            foreach (var p in unconsumed ?? new HuaweiPurchase[0])
            {
                var order = Map(p);
                if (order != null && p.PurchaseState == 0)
                    OrderPending?.Invoke(order);
            }
        }

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            m_Products = products ?? new BillingProductConfig[0];
            var result = new List<StoreProduct>();
            foreach (var type in AllTypes)
            {
                var ids = m_Products.Where(p => TypeOf(p.type) == type).Select(p => p.StoreId(Store)).Distinct().ToList();
                if (ids.Count == 0)
                    continue;
                var found = await TaskTimeout.With(m_Iap.GetProductsAsync(type, ids, ct), OperationTimeout, ct, "HMS product info");
                foreach (var p in found ?? new HuaweiProduct[0])
                {
                    var config = FindConfig(p.ProductId);
                    result.Add(new StoreProduct(config?.id ?? p.ProductId, p.ProductId, config?.type ?? TypeOf(p.PriceType), p.Price,
                        p.MicrosPrice / 1_000_000m, p.Currency, p.ProductName, p.ProductDesc, true, p, subscriptionPeriod: p.SubPeriod));
                }
            }
            return result;
        }

        /// <summary>The server binds nothing for Huawei: the ledger keeps consumables with their buyer.</summary>
        public void SetAccountBinding(string userId) { }

        /// <inheritdoc/>
        public void Purchase(string storeProductId) => _ = PurchaseAsync(storeProductId);

        private async Task PurchaseAsync(string storeProductId)
        {
            HuaweiPurchaseResult result;
            try
            {
                var config = FindConfig(storeProductId);
                var type = config != null ? TypeOf(config.type) : HuaweiPriceType.Consumable;
                result = await TaskTimeout.With(m_Iap.PurchaseAsync(storeProductId, type, "", m_Lifetime.Token), PurchaseTimeout,
                    m_Lifetime.Token, "HMS purchase");
            }
            catch (Exception e)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Huawei: " + e.Message, false));
                return;
            }
            var order = result?.ReturnCode == HuaweiStatus.Success ? Map(result.Purchase) : null;
            if (order != null)
            {
                OrderPending?.Invoke(order);
                return;
            }
            var code = result?.ReturnCode ?? -1;
            var reason = code == HuaweiStatus.ProductOwned ? "already owned: restore purchases"
                : code == HuaweiStatus.Cancelled ? "cancelled"
                : $"code {code} {result?.Message}".Trim();
            PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Huawei: " + reason, code == HuaweiStatus.Cancelled));
        }

        /// <summary>Consumes a consumable after the server's definitive answer; non-consumables and subscriptions need
        /// nothing.</summary>
        public void Finish(StoreOrder order, bool granted)
        {
            if (order == null)
                return;
            if (!(order.Native is HuaweiPurchase p) || p.PriceType != HuaweiPriceType.Consumable || string.IsNullOrEmpty(order.PurchaseToken))
            {
                OrderFinished?.Invoke(order, true, null);
                return;
            }
            _ = ConsumeAsync(order);
        }

        private async Task ConsumeAsync(StoreOrder order)
        {
            try
            {
                await TaskTimeout.With(Done(m_Iap.ConsumeAsync(order.PurchaseToken, m_Lifetime.Token)), OperationTimeout, m_Lifetime.Token, "HMS consume");
                OrderFinished?.Invoke(order, true, null);
            }
            catch (Exception e)
            {
                OrderFinished?.Invoke(order, false, "Huawei consume: " + e.Message);
            }
        }

        /// <summary>What HMS reports owned: consumables not consumed, non-consumables, active subscriptions.</summary>
        public async Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct)
        {
            var orders = new List<StoreOrder>();
            foreach (var type in AllTypes)
            {
                var owned = await TaskTimeout.With(m_Iap.GetOwnedPurchasesAsync(type, ct), OperationTimeout, ct, "HMS owned purchases");
                orders.AddRange((owned ?? new HuaweiPurchase[0]).Where(p => p.PurchaseState == 0).Select(Map).Where(o => o != null));
            }
            return orders;
        }

        /// <summary>Restoring on Huawei is <see cref="FetchPurchasesAsync"/>.</summary>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        /// <summary>Ends the calls still running.</summary>
        public void Dispose() => m_Lifetime.Cancel();

        /// <summary>The order the server verifies: the purchase token, and the order id (for subscriptions the
        /// subscription id, which the server looks them up by).</summary>
        internal StoreOrder Map(HuaweiPurchase p)
        {
            if (p == null || string.IsNullOrEmpty(p.PurchaseToken) || string.IsNullOrEmpty(p.ProductId))
                return null;
            var id = p.PriceType == HuaweiPriceType.Subscription ? p.SubscriptionId : p.OrderId;
            if (string.IsNullOrEmpty(id))
                return null;
            return new StoreOrder(Platform, p.ProductId, id, p.PurchaseToken, p);
        }

        private static async Task<bool> Done(Task task)
        {
            await task;
            return true;
        }

        private BillingProductConfig FindConfig(string productId)
        {
            foreach (var p in m_Products)
                if (p.StoreId(Store) == productId || p.Matches(productId))
                    return p;
            return null;
        }

        private static HuaweiPriceType TypeOf(BillingProductType type)
        {
            switch (type)
            {
                case BillingProductType.NonConsumable: return HuaweiPriceType.NonConsumable;
                case BillingProductType.Subscription: return HuaweiPriceType.Subscription;
                default: return HuaweiPriceType.Consumable;
            }
        }

        private static BillingProductType TypeOf(HuaweiPriceType type)
        {
            switch (type)
            {
                case HuaweiPriceType.NonConsumable: return BillingProductType.NonConsumable;
                case HuaweiPriceType.Subscription: return BillingProductType.Subscription;
                default: return BillingProductType.Consumable;
            }
        }
    }
}
