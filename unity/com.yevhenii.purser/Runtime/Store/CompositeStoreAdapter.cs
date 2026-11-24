using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>Picks the store a purchase goes through in <see cref="CompositeStoreAdapter"/>.</summary>
    public interface IStoreRouter
    {
        /// <summary>The store (<see cref="IStoreAdapter.Store"/>) for a purchase of <paramref name="product"/>; null or
        /// unknown: the main store. <paramref name="mainStoreProduct"/> is the product as the main store loaded it (its
        /// price and currency), null when it did not.</summary>
        string StoreFor(BillingProductConfig product, StoreProduct mainStoreProduct);
    }

    /// <summary>Sends purchases to another store when the main store shows the price in one of some currencies (e.g.
    /// <c>RUB</c> → <c>xsolla</c>: Google Play cannot charge players in Russia).</summary>
    public sealed class CurrencyStoreRouter : IStoreRouter
    {
        private readonly HashSet<string> m_Currencies;
        private readonly string m_Store;

        public CurrencyStoreRouter(string store, params string[] currencies)
        {
            m_Store = store;
            m_Currencies = new HashSet<string>((currencies ?? new string[0]).Select(c => c?.Trim().ToUpperInvariant()), StringComparer.Ordinal);
        }

        /// <inheritdoc/>
        public string StoreFor(BillingProductConfig product, StoreProduct mainStoreProduct) =>
            mainStoreProduct != null && !mainStoreProduct.IsFallbackPrice && mainStoreProduct.Currency != null &&
            m_Currencies.Contains(mainStoreProduct.Currency.ToUpperInvariant())
                ? m_Store
                : null;
    }

    /// <summary>A router from a function, e.g. a remote config flag or the debug menu.</summary>
    public sealed class FuncStoreRouter : IStoreRouter
    {
        private readonly Func<BillingProductConfig, StoreProduct, string> m_Pick;

        public FuncStoreRouter(Func<BillingProductConfig, StoreProduct, string> pick) =>
            m_Pick = pick ?? throw new ArgumentNullException(nameof(pick));

        /// <inheritdoc/>
        public string StoreFor(BillingProductConfig product, StoreProduct mainStoreProduct) => m_Pick(product, mainStoreProduct);
    }

    /// <summary>
    /// Several stores in one build: the main store (Google Play through Unity IAP, or <see cref="ExternalStoreAdapter"/>
    /// in observer mode) loads the products and prices, and <see cref="IStoreRouter"/> sends some purchases to another
    /// store (Xsolla Pay Station for players Google Play cannot charge). Every store's orders are verified at their own
    /// store's route with their own build (<see cref="BillingConfig.builds"/>); the customer info covers all builds of
    /// the app on the server.
    /// <code>
    /// var store = new CompositeStoreAdapter(Billing.CreateDefaultStore(config),
    ///     new CurrencyStoreRouter(BillingStores.Xsolla, "RUB"),
    ///     new XsollaStoreAdapter(config, new XsollaOptions { ReturnUrl = "mygame://purchase" }));
    /// </code>
    /// </summary>
    public sealed class CompositeStoreAdapter : IStoreAdapter, ICheckoutStoreAdapter, IStoreConnectionEvents, ISubscriptionChangeStore
    {
        private readonly IStoreAdapter m_Main;
        private readonly IStoreAdapter[] m_Others;
        private readonly IStoreRouter m_Router;
        private Dictionary<string, StoreProduct> m_MainProducts = new Dictionary<string, StoreProduct>();

        /// <param name="main">Loads the products and sells everything the router does not send elsewhere.</param>
        /// <param name="router">Picks the store of a purchase.</param>
        /// <param name="others">The other stores, by their <see cref="IStoreAdapter.Store"/>.</param>
        public CompositeStoreAdapter(IStoreAdapter main, IStoreRouter router, params IStoreAdapter[] others)
        {
            m_Main = main ?? throw new ArgumentNullException(nameof(main));
            m_Router = router ?? throw new ArgumentNullException(nameof(router));
            m_Others = (others ?? new IStoreAdapter[0]).Where(o => o != null).ToArray();
            if (m_Others.Any(o => o.Store == main.Store) || m_Others.Select(o => o.Store).Distinct().Count() != m_Others.Length)
                throw new ArgumentException("every store of a CompositeStoreAdapter must be another one");
            foreach (var store in Stores)
            {
                var owner = store;
                store.OrderPending += order => OrderPending?.Invoke(Owned(order, owner));
                store.OrderDeferred += order => OrderDeferred?.Invoke(Owned(order, owner));
                store.PurchaseFailed += failure => PurchaseFailed?.Invoke(failure);
                store.OrderFinished += (order, ok, error) => OrderFinished?.Invoke(order, ok, error);
            }
            if (m_Main is IStoreConnectionEvents connection)
                connection.Disconnected += reason => Disconnected?.Invoke(reason);
            if (m_Main is ISubscriptionChangeStore changes)
                changes.ChangeDeferred += id => ChangeDeferred?.Invoke(id);
        }

        private IEnumerable<IStoreAdapter> Stores => new[] { m_Main }.Concat(m_Others);

        private static StoreOrder Owned(StoreOrder order, IStoreAdapter owner)
        {
            if (order != null && string.IsNullOrEmpty(order.Store))
                order.Store = owner.Store;
            return order;
        }

        /// <summary>The store adapter whose order this is.</summary>
        internal IStoreAdapter OwnerOf(StoreOrder order) =>
            Stores.FirstOrDefault(s => s.Store == order?.Store) ?? m_Main;

        /// <summary>The adapter of a store id, or null.</summary>
        public IStoreAdapter Find(string store) => Stores.FirstOrDefault(s => s.Store == store);

        /// <summary>The store a purchase of <paramref name="product"/> goes through now.</summary>
        public string StoreFor(BillingProductConfig product)
        {
            m_MainProducts.TryGetValue(product.StoreId(m_Main.Store), out var loaded);
            var picked = m_Router.StoreFor(product, loaded);
            return Find(picked) != null ? picked : m_Main.Store;
        }

        /// <inheritdoc/>
        public BillingPlatform Platform => m_Main.Platform;

        /// <inheritdoc/>
        public string Store => m_Main.Store;

        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <inheritdoc/>
        public event Action<StoreOrder> OrderDeferred;
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<StoreOrder, bool, string> OrderFinished;
        /// <inheritdoc/>
        public event Action<string> Disconnected;
        /// <inheritdoc/>
        public event Action<string> ChangeDeferred;

        /// <inheritdoc/>
        public async Task ConnectAsync(CancellationToken ct)
        {
            await m_Main.ConnectAsync(ct);
            foreach (var other in m_Others)
            {
                try
                {
                    await other.ConnectAsync(ct);
                }
                catch (Exception e) when (!(e is OperationCanceledException))
                {
                    // The main store sells everything when another one cannot connect.
                    UnityEngine.Debug.LogWarning($"[Purser] {other.Store} could not connect: {e.Message}");
                }
            }
        }

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            var loaded = await m_Main.FetchProductsAsync(products, ct);
            m_MainProducts = loaded.GroupBy(p => p.StoreId).ToDictionary(g => g.Key, g => g.First());
            return loaded;
        }

        /// <inheritdoc/>
        public void SetAccountBinding(string userId)
        {
            foreach (var s in Stores)
                s.SetAccountBinding(userId);
        }

        /// <inheritdoc/>
        public void PrepareCheckout(CheckoutContext context)
        {
            foreach (var s in Stores.OfType<ICheckoutStoreAdapter>())
                s.PrepareCheckout(context);
        }

        /// <summary>Buys in the main store; <see cref="BillingService"/> routes through <see cref="PurchaseIn"/>.</summary>
        public void Purchase(string storeProductId) => m_Main.Purchase(storeProductId);

        /// <summary>Buys <paramref name="storeProductId"/> in the store <paramref name="store"/>.</summary>
        public void PurchaseIn(string store, string storeProductId) => (Find(store) ?? m_Main).Purchase(storeProductId);

        /// <inheritdoc/>
        public void PurchaseReplacing(string storeProductId, string replacedStoreProductId, ReplacementMode mode)
        {
            if (!(m_Main is ISubscriptionChangeStore changes))
                throw new NotSupportedException($"the {m_Main.Store} store adapter does not change subscriptions");
            changes.PurchaseReplacing(storeProductId, replacedStoreProductId, mode);
        }

        /// <inheritdoc/>
        public void Finish(StoreOrder order, bool granted) => OwnerOf(order).Finish(order, granted);

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct)
        {
            var all = new List<StoreOrder>();
            foreach (var s in Stores)
                all.AddRange((await s.FetchPurchasesAsync(ct)).Select(o => Owned(o, s)));
            return all;
        }

        /// <inheritdoc/>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => m_Main.RestoreTransactionsAsync(ct);

        /// <inheritdoc/>
        public void Dispose()
        {
            foreach (var s in Stores)
                s.Dispose();
        }
    }
}
