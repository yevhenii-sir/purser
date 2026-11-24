using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// The store adapter of observer mode: the game keeps its own store code and reports purchases with
    /// <see cref="IBillingService.ReportPurchaseAsync"/>; Purser does not start purchases
    /// (<see cref="IBillingService.PurchaseAsync(string, CancellationToken)"/> fails), but customer info,
    /// paywalls and the server's analytics work as usual. Products are the config's, with the prices the game sets
    /// (<see cref="SetPrice"/>) or the fallback prices.
    /// </summary>
    public sealed class ExternalStoreAdapter : IStoreAdapter
    {
        private readonly Dictionary<string, (decimal price, string currency, string localized)> m_Prices =
            new Dictionary<string, (decimal, string, string)>();

        /// <param name="store">The store the game sells through (<see cref="BillingStores"/>): picks the build of
        /// <see cref="BillingConfig.builds"/> and the product ids.</param>
        public ExternalStoreAdapter(string store, BillingPlatform platform)
        {
            Store = string.IsNullOrEmpty(store) ? throw new ArgumentException("store is required", nameof(store)) : store;
            Platform = platform;
        }

        /// <summary>For the device's platform: Google Play on Android, the App Store on Apple platforms.</summary>
        public static ExternalStoreAdapter ForThisDevice()
        {
#if UNITY_IOS || UNITY_TVOS || UNITY_STANDALONE_OSX || UNITY_VISIONOS
            return new ExternalStoreAdapter(BillingStores.Apple, BillingPlatform.Apple);
#else
            return new ExternalStoreAdapter(BillingStores.Google, BillingPlatform.Google);
#endif
        }

        /// <inheritdoc/>
        public BillingPlatform Platform { get; }

        /// <inheritdoc/>
        public string Store { get; }

        /// <summary>The store price of a product the game loaded itself, shown in <see cref="IBillingService.Products"/>.</summary>
        public void SetPrice(string storeProductId, decimal price, string currency, string localizedPrice = null)
        {
            if (!string.IsNullOrEmpty(storeProductId))
                m_Prices[storeProductId] = (price, currency, localizedPrice);
        }

#pragma warning disable 67 // Purchases come from ReportPurchaseAsync, not from store events.
        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <inheritdoc/>
        public event Action<StoreOrder> OrderDeferred;
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<StoreOrder, bool, string> OrderFinished;
#pragma warning restore 67

        /// <inheritdoc/>
        public Task ConnectAsync(CancellationToken ct) => Task.CompletedTask;

        /// <inheritdoc/>
        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(
            IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            var list = new List<StoreProduct>();
            foreach (var p in products)
            {
                var storeId = p.StoreId(Store);
                if (m_Prices.TryGetValue(storeId, out var known))
                {
                    list.Add(new StoreProduct(p.id, storeId, p.type, known.localized ?? $"{known.price} {known.currency}".Trim(), known.price,
                        known.currency, p.id, "", true, subscriptionPeriod: p.fallbackPeriod, isFallbackPeriod: true));
                    continue;
                }
                var price = (decimal)p.fallbackPrice;
                list.Add(new StoreProduct(p.id, storeId, p.type, price > 0 ? $"{price} {p.fallbackCurrency}".Trim() : "", price,
                    p.fallbackCurrency, p.id, "", true, isFallbackPrice: true, subscriptionPeriod: p.fallbackPeriod, isFallbackPeriod: true));
            }
            return Task.FromResult<IReadOnlyList<StoreProduct>>(list);
        }

        /// <inheritdoc/>
        public void SetAccountBinding(string userId)
        {
        }

        /// <summary>Not available in observer mode: the game's own store code buys.</summary>
        public void Purchase(string storeProductId) =>
            throw new NotSupportedException("observer mode: the game buys with its own store code and reports the purchase " +
                                            "with ReportPurchaseAsync");

        /// <inheritdoc/>
        public void Finish(StoreOrder order, bool granted)
        {
        }

        /// <inheritdoc/>
        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<StoreOrder>>(new StoreOrder[0]);

        /// <inheritdoc/>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) =>
            Task.FromResult(false);

        /// <inheritdoc/>
        public void Dispose()
        {
        }
    }
}
