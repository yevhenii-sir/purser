using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>Platform SDK <c>ProductType</c> of a Meta Horizon Store add-on.</summary>
    public enum MetaQuestProductType
    {
        /// <summary><c>CONSUMABLE</c>.</summary>
        Consumable,
        /// <summary><c>DURABLE</c> (a non-consumable).</summary>
        Durable,
        /// <summary><c>SUBSCRIPTION</c>.</summary>
        Subscription,
    }

    /// <summary>A Platform SDK <c>Product</c>.</summary>
    public sealed class MetaQuestProduct
    {
        public string Sku;
        public MetaQuestProductType Type;
        public string Name;
        public string Description;
        /// <summary>The price as Meta formats it, e.g. <c>$1.99</c>.</summary>
        public string FormattedPrice;
        /// <summary><c>Price.AmountInHundredths</c>: 199 for 1.99.</summary>
        public long AmountInHundredths;
        /// <summary>ISO 4217 currency.</summary>
        public string Currency;
    }

    /// <summary>A Platform SDK <c>Purchase</c> (the fields the billing server needs).</summary>
    public sealed class MetaQuestPurchase
    {
        /// <summary><c>Purchase.ID</c>; <c>0</c> for shared add-on entitlements.</summary>
        public string Id;
        public string Sku;
        public MetaQuestProductType Type;
        /// <summary><c>GrantTime</c> (UTC).</summary>
        public DateTime GrantTime;
    }

    /// <summary>The outcome of <see cref="IMetaQuestIap.PurchaseAsync"/>: a purchase, or why there is none.</summary>
    public sealed class MetaQuestPurchaseResult
    {
        /// <summary>Set when the player paid.</summary>
        public MetaQuestPurchase Purchase;
        /// <summary>The player closed the checkout.</summary>
        public bool Cancelled;
        /// <summary>The Platform SDK's error message otherwise.</summary>
        public string Error;
    }

    /// <summary>
    /// The Meta Platform SDK calls the billing package needs. The Platform SDK is not a dependency of the package, so the
    /// game provides this bridge (a ready-made one over <c>Oculus.Platform</c> is in the "Meta Quest" sample). Tasks
    /// complete on the Unity main thread. A failed call throws.
    /// </summary>
    public interface IMetaQuestIap
    {
        /// <summary>The Platform SDK is initialized (<c>Core.AsyncInitialize</c>; the entitlement check is the game's);
        /// answers the logged-in user's id (<c>Users.GetLoggedInUser</c>, <c>User.ID</c>).</summary>
        Task<string> InitializeAsync(CancellationToken ct);
        /// <summary><c>Users.GetUserProof</c>: a nonce the server checks with Meta, good for one check.</summary>
        Task<string> GetUserProofAsync(CancellationToken ct);
        /// <summary><c>IAP.GetProductsBySKU</c>.</summary>
        Task<IReadOnlyList<MetaQuestProduct>> GetProductsAsync(IReadOnlyList<string> skus, CancellationToken ct);
        /// <summary><c>IAP.LaunchCheckoutFlow</c>; never consumes the purchase.</summary>
        Task<MetaQuestPurchaseResult> PurchaseAsync(string sku, CancellationToken ct);
        /// <summary><c>IAP.GetViewerPurchases</c> (every page): durables, consumables not consumed yet, subscriptions.</summary>
        Task<IReadOnlyList<MetaQuestPurchase>> GetPurchasesAsync(CancellationToken ct);
        /// <summary><c>IAP.ConsumePurchase</c>.</summary>
        Task ConsumeAsync(string sku, CancellationToken ct);
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for the Meta Horizon Store (Meta Quest). The billing server checks the player's user
    /// proof and looks purchases up in Meta's server APIs (<c>/verify/meta</c>: the Meta user id, a new proof per request
    /// through <see cref="IStoreUserProof"/>, the purchase id). The server consumes consumables when it delivers the
    /// reward; when the game grants it (<c>delivery: client</c>) this adapter consumes them after the server's answer.
    /// Pass it in <see cref="BillingDependencies.Store"/>:
    /// <code>new BillingDependencies { Store = new MetaQuestStoreAdapter(new MetaQuestPlatformBridge()) }</code>
    /// </summary>
    public sealed class MetaQuestStoreAdapter : IStoreAdapter, IStoreUserProof
    {
        private static readonly TimeSpan OperationTimeout = TimeSpan.FromSeconds(30);
        /// <summary>A checkout that never answered fails after this long instead of waiting forever.</summary>
        private static readonly TimeSpan PurchaseTimeout = TimeSpan.FromMinutes(15);

        private readonly IMetaQuestIap m_Iap;
        private readonly BillingLog m_Log;
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        private IReadOnlyList<BillingProductConfig> m_Products = new BillingProductConfig[0];
        private string m_UserId;

        /// <inheritdoc/>
        public BillingPlatform Platform => BillingPlatform.MetaQuest;
        /// <inheritdoc/>
        public string Store => BillingStores.Meta;

        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <summary>Meta has no deferred payments.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<StoreOrder, bool, string> OrderFinished;

        /// <param name="iap">Bridge to the Meta Platform SDK.</param>
        public MetaQuestStoreAdapter(IMetaQuestIap iap, BillingLog log = null)
        {
            m_Iap = iap ?? throw new ArgumentNullException(nameof(iap));
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
        }

        /// <summary>Initializes the Platform SDK and reads the user, then raises the consumables bought and not consumed
        /// yet (a purchase whose answer never came) through <see cref="OrderPending"/>.</summary>
        public async Task ConnectAsync(CancellationToken ct)
        {
            var user = await TaskTimeout.With(m_Iap.InitializeAsync(ct), OperationTimeout, ct, "Meta Platform SDK");
            if (string.IsNullOrEmpty(user) || user == "0")
                throw new BillingException("Meta Platform SDK: no logged-in user (the app needs the User ID Data Use Checkup)");
            m_UserId = user;
            IReadOnlyList<MetaQuestPurchase> owned;
            try
            {
                owned = await TaskTimeout.With(m_Iap.GetPurchasesAsync(ct), OperationTimeout, ct, "Meta purchases");
            }
            catch (BillingException e)
            {
                m_Log.Warning("Meta purchases at startup: " + e.Message);
                return;
            }
            foreach (var p in owned ?? new MetaQuestPurchase[0])
            {
                var order = p?.Type == MetaQuestProductType.Consumable ? Map(p) : null;
                if (order != null)
                    OrderPending?.Invoke(order);
            }
        }

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            m_Products = products ?? new BillingProductConfig[0];
            var skus = m_Products.Select(p => p.StoreId(Store)).Distinct().ToList();
            var result = new List<StoreProduct>();
            if (skus.Count == 0)
                return result;
            var found = await TaskTimeout.With(m_Iap.GetProductsAsync(skus, ct), OperationTimeout, ct, "Meta products");
            foreach (var p in found ?? new MetaQuestProduct[0])
            {
                var config = FindConfig(p.Sku);
                result.Add(new StoreProduct(config?.id ?? p.Sku, p.Sku, config?.type ?? TypeOf(p.Type), p.FormattedPrice,
                    p.AmountInHundredths / 100m, p.Currency, p.Name, p.Description, true, p));
            }
            return result;
        }

        /// <summary>Meta has no account field: the server checks the user with a proof instead.</summary>
        public void SetAccountBinding(string userId) { }

        /// <summary>A new user proof (<c>Users.GetUserProof</c>) for one request.</summary>
        public Task<string> GetUserProofAsync(CancellationToken ct) =>
            TaskTimeout.With(m_Iap.GetUserProofAsync(ct), OperationTimeout, ct, "Meta user proof");

        /// <inheritdoc/>
        public void Purchase(string storeProductId) => _ = PurchaseAsync(storeProductId);

        private async Task PurchaseAsync(string sku)
        {
            MetaQuestPurchaseResult result;
            try
            {
                result = await TaskTimeout.With(m_Iap.PurchaseAsync(sku, m_Lifetime.Token), PurchaseTimeout, m_Lifetime.Token, "Meta checkout");
            }
            catch (Exception e)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Meta: " + e.Message, false));
                return;
            }
            var order = result?.Purchase != null ? Map(result.Purchase) : null;
            if (order != null)
            {
                OrderPending?.Invoke(order);
                return;
            }
            var cancelled = result == null || result.Cancelled;
            PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Meta: " + (cancelled ? "cancelled" : result.Error ?? "no purchase"), cancelled));
        }

        /// <summary>Consumes a consumable the server accepted (when the server delivered the reward it consumed it
        /// already, and this consume fails harmlessly). A purchase the server refused is left as it is: the refusal may be
        /// the server's setup, and the next launch verifies it again.</summary>
        public void Finish(StoreOrder order, bool granted)
        {
            if (order == null || !granted)
                return;
            if (!(order.Native is MetaQuestPurchase p) || p.Type != MetaQuestProductType.Consumable)
            {
                OrderFinished?.Invoke(order, true, null);
                return;
            }
            _ = ConsumeAsync(order, p.Sku);
        }

        private async Task ConsumeAsync(StoreOrder order, string sku)
        {
            try
            {
                await TaskTimeout.With(Done(m_Iap.ConsumeAsync(sku, m_Lifetime.Token)), OperationTimeout, m_Lifetime.Token, "Meta consume");
                OrderFinished?.Invoke(order, true, null);
            }
            catch (Exception e)
            {
                OrderFinished?.Invoke(order, false, "Meta consume: " + e.Message);
            }
        }

        /// <summary>What Meta reports owned: durables, consumables not consumed, subscriptions.</summary>
        public async Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct)
        {
            var owned = await TaskTimeout.With(m_Iap.GetPurchasesAsync(ct), OperationTimeout, ct, "Meta purchases");
            return (owned ?? new MetaQuestPurchase[0]).Select(Map).Where(o => o != null).ToList();
        }

        /// <summary>Restoring on Meta is <see cref="FetchPurchasesAsync"/>.</summary>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        /// <summary>Ends the calls still running.</summary>
        public void Dispose() => m_Lifetime.Cancel();

        /// <summary>The order the server verifies: the SKU, the purchase id (shared entitlements, whose id is 0, get the
        /// SKU and grant time as their key) and the Meta user id.</summary>
        internal StoreOrder Map(MetaQuestPurchase p)
        {
            if (p == null || string.IsNullOrEmpty(p.Sku) || string.IsNullOrEmpty(m_UserId))
                return null;
            var id = string.IsNullOrEmpty(p.Id) || p.Id == "0"
                ? p.Sku + "." + new DateTimeOffset(DateTime.SpecifyKind(p.GrantTime, DateTimeKind.Utc)).ToUnixTimeSeconds()
                : p.Id;
            return new StoreOrder(Platform, p.Sku, id, null, p, m_UserId);
        }

        private static async Task<bool> Done(Task task)
        {
            await task;
            return true;
        }

        private BillingProductConfig FindConfig(string sku)
        {
            foreach (var p in m_Products)
                if (p.StoreId(Store) == sku || p.Matches(sku))
                    return p;
            return null;
        }

        private static BillingProductType TypeOf(MetaQuestProductType type)
        {
            switch (type)
            {
                case MetaQuestProductType.Durable: return BillingProductType.NonConsumable;
                case MetaQuestProductType.Subscription: return BillingProductType.Subscription;
                default: return BillingProductType.Consumable;
            }
        }
    }
}
