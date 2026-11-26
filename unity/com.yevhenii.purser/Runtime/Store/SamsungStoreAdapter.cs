using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>Response codes of Samsung IAP (<c>ErrorVo.errorCode</c>).</summary>
    public static class SamsungStatus
    {
        /// <summary><c>IAP_ERROR_NONE</c>.</summary>
        public const int Success = 0;
        /// <summary><c>IAP_PAYMENT_IS_CANCELED</c>: the player closed the payment screen.</summary>
        public const int Cancelled = 1;
        /// <summary><c>IAP_ERROR_ALREADY_PURCHASED</c>: the player owns it (a consumable not consumed yet, a non-consumable).</summary>
        public const int AlreadyPurchased = -1003;
    }

    /// <summary>A Samsung IAP <c>ProductVo</c> (the fields the billing package needs).</summary>
    public sealed class SamsungProduct
    {
        /// <summary><c>mItemId</c>.</summary>
        public string ItemId;
        /// <summary><c>mItemPriceString</c>: the price as Samsung formats it, e.g. <c>€1.99</c>.</summary>
        public string PriceString;
        /// <summary><c>mItemPrice</c>.</summary>
        public decimal Price;
        /// <summary><c>mCurrencyCode</c>, ISO 4217.</summary>
        public string Currency;
        public string Name;
        public string Description;
        /// <summary>Subscriptions: the period as ISO 8601 (<c>P1M</c>), from <c>mSubscriptionDurationUnit</c> and
        /// <c>mSubscriptionDurationMultiplier</c>; null when unknown.</summary>
        public string SubscriptionPeriod;
    }

    /// <summary>A Samsung IAP <c>PurchaseVo</c> or <c>OwnedProductVo</c> (the fields the billing server needs).</summary>
    public sealed class SamsungPurchase
    {
        /// <summary><c>mPurchaseId</c>: what the server looks the purchase up by.</summary>
        public string PurchaseId;
        /// <summary><c>mOrderId</c>; owned purchases do not carry it.</summary>
        public string OrderId;
        /// <summary><c>mItemId</c>.</summary>
        public string ItemId;
    }

    /// <summary>The outcome of <see cref="ISamsungIap.PurchaseAsync"/>.</summary>
    public sealed class SamsungPurchaseResult
    {
        /// <summary><see cref="SamsungStatus"/> or another Samsung IAP code.</summary>
        public int ErrorCode;
        public string Message;
        /// <summary>The purchase when <see cref="ErrorCode"/> is <see cref="SamsungStatus.Success"/>.</summary>
        public SamsungPurchase Purchase;
    }

    /// <summary>
    /// The Samsung IAP calls the billing package needs. Unity IAP has no Samsung store, so the game provides this bridge
    /// over Samsung's IAP Unity plugin (a ready-made bridge is in the "Samsung Galaxy Store" sample). Tasks complete on
    /// the Unity main thread. A failed call throws.
    /// </summary>
    public interface ISamsungIap
    {
        /// <summary>Sets the operation mode (production or test) and checks that IAP is available.</summary>
        Task InitializeAsync(CancellationToken ct);
        /// <summary><c>GetProductsDetails</c> of these items.</summary>
        Task<IReadOnlyList<SamsungProduct>> GetProductsAsync(IReadOnlyList<string> itemIds, CancellationToken ct);
        /// <summary><c>StartPayment</c>; never consumes the purchase.</summary>
        Task<SamsungPurchaseResult> PurchaseAsync(string itemId, CancellationToken ct);
        /// <summary><c>GetOwnedList</c> of every type: consumables not consumed yet, non-consumables, active
        /// subscriptions.</summary>
        Task<IReadOnlyList<SamsungPurchase>> GetOwnedPurchasesAsync(CancellationToken ct);
        /// <summary><c>ConsumePurchasedItems</c> of one purchase.</summary>
        Task ConsumeAsync(string purchaseId, CancellationToken ct);
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for Samsung Galaxy Store. The billing server looks purchases up with Samsung's APIs
    /// (<c>/verify/samsung</c>: the purchase id) and consumes consumables and acknowledges subscriptions itself; after a
    /// granted answer the adapter consumes consumables too, for builds whose reward the client grants (a consume the
    /// server did already is reported as failed and changes nothing). Pass it in <see cref="BillingDependencies.Store"/>:
    /// <code>new BillingDependencies { Store = new SamsungStoreAdapter(new SamsungIapBridge()) }</code>
    /// </summary>
    public sealed class SamsungStoreAdapter : IStoreAdapter
    {
        private static readonly TimeSpan OperationTimeout = TimeSpan.FromSeconds(30);
        /// <summary>A purchase whose payment screen never answered fails after this long instead of waiting forever.</summary>
        private static readonly TimeSpan PurchaseTimeout = TimeSpan.FromMinutes(15);

        private readonly ISamsungIap m_Iap;
        private readonly BillingLog m_Log;
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        private IReadOnlyList<BillingProductConfig> m_Products = new BillingProductConfig[0];
        /// <summary>Owned purchases found while connecting, before the products (which say what is consumable) are known.</summary>
        private List<StoreOrder> m_Owned = new List<StoreOrder>();

        /// <inheritdoc/>
        public BillingPlatform Platform => BillingPlatform.Samsung;
        /// <inheritdoc/>
        public string Store => BillingStores.Samsung;

        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <summary>Samsung IAP has no deferred payments.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<StoreOrder, bool, string> OrderFinished;

        /// <param name="iap">Bridge to the Samsung IAP Unity plugin.</param>
        public SamsungStoreAdapter(ISamsungIap iap, BillingLog log = null)
        {
            m_Iap = iap ?? throw new ArgumentNullException(nameof(iap));
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
        }

        /// <summary>Checks that IAP is available, then raises the consumables bought and not consumed yet (a purchase
        /// whose answer never came) through <see cref="OrderPending"/>: at once when the products are known, otherwise
        /// once <see cref="FetchProductsAsync"/> loaded them (Samsung reports consumables and non-consumables alike).</summary>
        public async Task ConnectAsync(CancellationToken ct)
        {
            await TaskTimeout.With(Done(m_Iap.InitializeAsync(ct)), OperationTimeout, ct, "Samsung IAP");
            IReadOnlyList<SamsungPurchase> owned;
            try
            {
                owned = await TaskTimeout.With(m_Iap.GetOwnedPurchasesAsync(ct), OperationTimeout, ct, "Samsung owned purchases");
            }
            catch (BillingException e)
            {
                m_Log.Warning("Samsung owned purchases at startup: " + e.Message);
                return;
            }
            m_Owned = (owned ?? new SamsungPurchase[0]).Select(Map).Where(o => o != null).ToList();
            if (m_Products.Count > 0)
                RaiseUnconsumed();
        }

        private void RaiseUnconsumed()
        {
            var owned = m_Owned;
            m_Owned = new List<StoreOrder>();
            foreach (var order in owned.Where(o => IsConsumable(o.StoreProductId)))
                OrderPending?.Invoke(order);
        }

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            m_Products = products ?? new BillingProductConfig[0];
            var ids = m_Products.Select(p => p.StoreId(Store)).Distinct().ToList();
            var result = new List<StoreProduct>();
            if (ids.Count > 0)
            {
                var found = await TaskTimeout.With(m_Iap.GetProductsAsync(ids, ct), OperationTimeout, ct, "Samsung product details");
                foreach (var p in found ?? new SamsungProduct[0])
                {
                    var config = FindConfig(p.ItemId);
                    if (config == null)
                        continue;
                    result.Add(new StoreProduct(config.id, p.ItemId, config.type, p.PriceString, p.Price, p.Currency, p.Name,
                        p.Description, true, p, subscriptionPeriod: p.SubscriptionPeriod));
                }
            }
            RaiseUnconsumed();
            return result;
        }

        /// <summary>The server binds nothing for Samsung: the ledger keeps consumables with their buyer.</summary>
        public void SetAccountBinding(string userId) { }

        /// <inheritdoc/>
        public void Purchase(string storeProductId) => _ = PurchaseAsync(storeProductId);

        private async Task PurchaseAsync(string storeProductId)
        {
            SamsungPurchaseResult result;
            try
            {
                result = await TaskTimeout.With(m_Iap.PurchaseAsync(storeProductId, m_Lifetime.Token), PurchaseTimeout, m_Lifetime.Token,
                    "Samsung purchase");
            }
            catch (Exception e)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Samsung: " + e.Message, false));
                return;
            }
            var order = result?.ErrorCode == SamsungStatus.Success ? Map(result.Purchase) : null;
            if (order != null)
            {
                OrderPending?.Invoke(order);
                return;
            }
            var code = result?.ErrorCode ?? -1;
            var reason = code == SamsungStatus.AlreadyPurchased ? "already owned: restore purchases"
                : code == SamsungStatus.Cancelled ? "cancelled"
                : $"code {code} {result?.Message}".Trim();
            PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "Samsung: " + reason, code == SamsungStatus.Cancelled));
        }

        /// <summary>Consumes a granted consumable. A refused one is left unconsumed: the refusal may be the server's setup,
        /// and the purchase is verified again on the next start. Non-consumables and subscriptions need nothing here (the
        /// server acknowledges them).</summary>
        public void Finish(StoreOrder order, bool granted)
        {
            if (order == null)
                return;
            if (!granted || !IsConsumable(order.StoreProductId) || string.IsNullOrEmpty(order.PurchaseToken))
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
                await TaskTimeout.With(Done(m_Iap.ConsumeAsync(order.PurchaseToken, m_Lifetime.Token)), OperationTimeout, m_Lifetime.Token,
                    "Samsung consume");
                OrderFinished?.Invoke(order, true, null);
            }
            catch (Exception e)
            {
                OrderFinished?.Invoke(order, false, "Samsung consume: " + e.Message);
            }
        }

        /// <summary>What Samsung reports owned: consumables not consumed, non-consumables, active subscriptions.</summary>
        public async Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct)
        {
            var owned = await TaskTimeout.With(m_Iap.GetOwnedPurchasesAsync(ct), OperationTimeout, ct, "Samsung owned purchases");
            return (owned ?? new SamsungPurchase[0]).Select(Map).Where(o => o != null).ToList();
        }

        /// <summary>Restoring on Samsung is <see cref="FetchPurchasesAsync"/>.</summary>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        /// <summary>Ends the calls still running.</summary>
        public void Dispose() => m_Lifetime.Cancel();

        /// <summary>The order the server verifies: the purchase id as its token, and the order id when Samsung gives
        /// one.</summary>
        internal StoreOrder Map(SamsungPurchase p)
        {
            if (p == null || string.IsNullOrEmpty(p.PurchaseId) || string.IsNullOrEmpty(p.ItemId))
                return null;
            return new StoreOrder(Platform, p.ItemId, p.OrderId ?? "", p.PurchaseId, p);
        }

        private static async Task<bool> Done(Task task)
        {
            await task;
            return true;
        }

        /// <summary>Samsung reports consumables and non-consumables alike as <c>item</c>: the config says which.</summary>
        private bool IsConsumable(string itemId) => FindConfig(itemId)?.type == BillingProductType.Consumable;

        private BillingProductConfig FindConfig(string itemId)
        {
            foreach (var p in m_Products)
                if (p.StoreId(Store) == itemId || p.Matches(itemId))
                    return p;
            return null;
        }
    }
}
