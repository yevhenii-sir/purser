using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Purchases: starting them in the store and reporting their results to the game. The orders the store delivers
    // are verified by OrderProcessor, which reports through IOrderProcessorHost.
    public sealed partial class BillingService : IOrderProcessorHost
    {
        /// <summary>A <c>PurchaseAsync</c> call waiting for the result of the purchase it started.</summary>
        private sealed class PurchaseWaiter : TaskCompletionSource<PurchaseResult>
        {
            /// <summary>The orders the service had when the purchase started (<see cref="OrderProcessor.KnownOrders"/>):
            /// their results are not the result of this purchase.</summary>
            public readonly HashSet<string> Older;

            public PurchaseWaiter(HashSet<string> older)
            {
                Older = older;
            }
        }

        /// <summary>Registers the caller of a purchase of <paramref name="storeId"/> that starts now.</summary>
        private PurchaseWaiter StartWaiting(string storeId)
        {
            var waiter = new PurchaseWaiter(m_Orders.KnownOrders());
            m_Waiters[storeId] = waiter;
            return waiter;
        }

        /// <inheritdoc/>
        public StoreProduct GetProduct(string productId)
        {
            if (productId == null)
                return null;
            return m_ProductsById.TryGetValue(productId, out var product) || m_ProductsByStoreId.TryGetValue(productId, out product)
                ? product
                : null;
        }

        /// <inheritdoc/>
        public Task<PurchaseResult> PurchaseAsync(string productId, CancellationToken ct = default) =>
            PurchaseCoreAsync(productId, null, ct);

        /// <inheritdoc/>
        public Task<PurchaseResult> PurchaseAsync(string productId, Paywall paywall, CancellationToken ct = default) =>
            PurchaseCoreAsync(productId, new PurchaseOptions { Paywall = paywall }, ct);

        /// <inheritdoc/>
        public Task<PurchaseResult> PurchaseAsync(string productId, PurchaseOptions options, CancellationToken ct = default) =>
            PurchaseCoreAsync(productId, options, ct);

        private async Task<PurchaseResult> PurchaseCoreAsync(string productId, PurchaseOptions options, CancellationToken ct)
        {
            if (!IsInitialized || m_Context.IsDisposed)
                return new PurchaseResult(PurchaseStatus.NotInitialized, productId, message: "billing is not initialized");

            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId);
            if (config == null)
                return new PurchaseResult(PurchaseStatus.Failed, productId, message: "unknown product");
            // Several stores (CompositeStoreAdapter): the router picks this purchase's store.
            var composite = m_Store as CompositeStoreAdapter;
            var store = composite?.StoreFor(config) ?? m_Store.Store;
            var storeId = config.StoreId(store);
            if (SyncUser() == null)
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: "user id is not set");
            if (m_Waiters.ContainsKey(storeId))
                return new PurchaseResult(PurchaseStatus.AlreadyInProgress, config.id, storeId);

            string replacedStoreId = null;
            if (!string.IsNullOrEmpty(options?.ReplacesProductId))
            {
                var replaced = m_Config.FindById(options.ReplacesProductId) ?? m_Config.FindByStoreId(options.ReplacesProductId);
                string problem = null;
                if (replaced == null)
                    problem = $"unknown product to replace: {options.ReplacesProductId}";
                else if (config.type != BillingProductType.Subscription || replaced.type != BillingProductType.Subscription)
                    problem = "only a subscription can replace a subscription";
                else if (replaced.StoreId(m_Store.Store) == storeId)
                    problem = "the subscription cannot replace itself";
                else if (!(m_Store is ISubscriptionChangeStore))
                    problem = $"the {m_Store.Store} store adapter does not change subscriptions";
                if (problem != null)
                    return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: problem);
                replacedStoreId = replaced.StoreId(m_Store.Store);
            }

            BindAccount();
            var waiter = StartWaiting(storeId);
            m_Orders.SetPurchaseContext(storeId, options?.Paywall, options?.OfferId);
            try
            {
                if (m_Store is ICheckoutStoreAdapter checkout)
                    checkout.PrepareCheckout(new CheckoutContext { UserId = m_Context.User, Attribution = await m_Context.AttributionAsync() });
                if (m_Context.IsDisposed)
                    return new PurchaseResult(PurchaseStatus.NotInitialized, config.id, storeId, message: "billing was disposed");
                if (replacedStoreId != null)
                    ((ISubscriptionChangeStore)m_Store).PurchaseReplacing(storeId, replacedStoreId, options.ReplacementMode);
                else if (composite != null)
                    composite.PurchaseIn(store, storeId);
                else
                    m_Store.Purchase(storeId);
            }
            catch (Exception e)
            {
                m_Waiters.Remove(storeId);
                m_Orders.ClearPurchaseContext(storeId);
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: e.Message);
            }

            return await AwaitResultAsync(storeId, waiter, ct);
        }

        /// <summary>Waits for the result of the store purchase. Cancelling <paramref name="ct"/> stops waiting only: the
        /// purchase goes on in the store and keeps its paywall and offer for the server.</summary>
        private async Task<PurchaseResult> AwaitResultAsync(string storeId, PurchaseWaiter waiter, CancellationToken ct)
        {
            using (ct.Register(() => waiter.TrySetCanceled()))
            {
                try
                {
                    return await waiter.Task;
                }
                finally
                {
                    if (m_Waiters.TryGetValue(storeId, out var current) && current == waiter)
                        m_Waiters.Remove(storeId);
                }
            }
        }

        private void OnOrderDeferred(StoreOrder order)
        {
            var config = m_Config.FindByStoreId(order.StoreProductId);
            Report(new PurchaseResult(PurchaseStatus.PendingPayment, config?.id ?? order.StoreProductId,
                order.StoreProductId, order.TransactionId, "waiting for approval"));
        }

        private void OnSubscriptionChangeDeferred(string storeProductId)
        {
            if (m_Context.IsDisposed)
                return;
            var config = m_Config.FindByStoreId(storeProductId);
            m_Orders.ClearPurchaseContext(storeProductId);
            Report(new PurchaseResult(PurchaseStatus.PendingPayment, config?.id ?? storeProductId, storeProductId,
                message: "the subscription change takes effect at the next renewal"));
            m_Refresher.RefreshInBackground();
        }

        private void OnStorePurchaseFailed(StorePurchaseFailure failure)
        {
            var status = failure.UserCancelled ? PurchaseStatus.Cancelled : PurchaseStatus.Failed;
            if (string.IsNullOrEmpty(failure.StoreProductId))
            {
                // The store did not say which purchase failed: fail every open one rather than leave them waiting.
                if (m_Waiters.Count == 0)
                    Report(new PurchaseResult(status, null, null, message: failure.Reason));
                foreach (var open in m_Waiters.Keys.ToList())
                    Report(new PurchaseResult(status, m_Config.FindByStoreId(open)?.id ?? open, open, message: failure.Reason));
                return;
            }
            var storeId = failure.StoreProductId;
            Report(new PurchaseResult(status, m_Config.FindByStoreId(storeId)?.id ?? storeId, storeId, message: failure.Reason));
        }

        /// <summary>
        /// Completes the purchase's waiter and raises its event. <see cref="ReportOutcome.HandlerFailed"/> when a game
        /// handler threw (logged, never passed on to the store callbacks).
        /// </summary>
        /// <param name="order">The order the result is about; null when the store reported the purchase without one.
        /// An order the service already had when a purchase of the product started is not the result of that purchase:
        /// its waiter keeps waiting.</param>
        private ReportOutcome Report(PurchaseResult result, StoreOrder order = null)
        {
            var received = false;
            if (!string.IsNullOrEmpty(result.StoreProductId))
            {
                if (m_Waiters.TryGetValue(result.StoreProductId, out var waiter) &&
                    (order == null || !OrderProcessor.IsKnown(waiter.Older, order)))
                {
                    m_Waiters.Remove(result.StoreProductId);
                    received = waiter.TrySetResult(result);
                }
                // The purchase ended without an order: its paywall and offer are not sent with later orders of the
                // product. A pending one keeps them for the order the store delivers later; OrderProcessor releases
                // those of an order.
                if (order == null && result.Status != PurchaseStatus.PendingPayment && result.Status != PurchaseStatus.PendingVerification)
                    m_Orders.ClearPurchaseContext(result.StoreProductId);
            }

            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info("Purchase " + result);
            Action<PurchaseResult> handlers;
            string name;
            switch (result.Status)
            {
                case PurchaseStatus.Success:
                case PurchaseStatus.PendingDelivery:
                    (handlers, name) = (m_PurchaseSucceeded, nameof(PurchaseSucceeded));
                    break;
                case PurchaseStatus.PendingPayment:
                case PurchaseStatus.PendingVerification:
                    (handlers, name) = (PurchasePending, nameof(PurchasePending));
                    break;
                default:
                    (handlers, name) = (PurchaseFailed, nameof(PurchaseFailed));
                    break;
            }
            try
            {
                handlers?.Invoke(result);
                return received || handlers != null ? ReportOutcome.Received : ReportOutcome.NoReceiver;
            }
            catch (Exception e)
            {
                m_Log.Error($"{name} handler threw for {result}: {e}");
                return ReportOutcome.HandlerFailed;
            }
        }

        // ---- IOrderProcessorHost (its Products is the public property) ----

        Task IOrderProcessorHost.ProductsFetch => m_ProductsFetch?.Task;

        string IOrderProcessorHost.SyncUser() => SyncUser();

        ReportOutcome IOrderProcessorHost.Report(PurchaseResult result, StoreOrder order) => Report(result, order);
    }
}
