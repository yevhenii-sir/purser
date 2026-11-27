using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// Stands in for a default store that could not be created (no Unity IAP, an Amazon build without its adapter): the
    /// service still exists, so a DI container builds, and <see cref="BillingService.InitializeAsync"/> fails with
    /// <see cref="Error"/>.
    /// </summary>
    internal sealed class UnavailableStoreAdapter : IStoreAdapter
    {
        public UnavailableStoreAdapter(BillingPlatform platform, BillingException error)
        {
            Platform = platform;
            Store = BillingStores.Of(platform) ?? "none";
            Error = error ?? throw new ArgumentNullException(nameof(error));
        }

        /// <summary>Why the store could not be created.</summary>
        public BillingException Error { get; }

        public BillingPlatform Platform { get; }

        public string Store { get; }

        public event Action<StoreOrder> OrderPending { add { } remove { } }

        public event Action<StoreOrder> OrderDeferred { add { } remove { } }

        public event Action<StorePurchaseFailure> PurchaseFailed { add { } remove { } }

        public event Action<StoreOrder, bool, string> OrderFinished { add { } remove { } }

        public Task ConnectAsync(CancellationToken ct) => Task.FromException(Error);

        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct) =>
            Task.FromException<IReadOnlyList<StoreProduct>>(Error);

        public void SetAccountBinding(string userId)
        {
        }

        public void Purchase(string storeProductId) => throw Error;

        public void Finish(StoreOrder order, bool granted)
        {
        }

        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromException<IReadOnlyList<StoreOrder>>(Error);

        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromException<bool>(Error);

        public void Dispose()
        {
        }
    }
}
