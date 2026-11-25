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
        /// <summary>A <c>PurchaseAsync</c> call waiting for the result of the purchase it started. Its task is what the
        /// caller awaits (no async method in between): the caller's code after the await is the first thing to run when
        /// the result is set, before the order is finished in the store (<see cref="ReportOutcome.Awaited"/>).</summary>
        private sealed class PurchaseWaiter : TaskCompletionSource<PurchaseResult>
        {
            /// <summary>The orders the service had when the purchase started (<see cref="OrderProcessor.KnownOrders"/>):
            /// their results are not the result of this purchase.</summary>
            public readonly HashSet<string> Older;

            /// <summary>The caller's cancellation token, released with the result.</summary>
            public CancellationTokenRegistration Cancellation;

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

        private Task<PurchaseResult> PurchaseCoreAsync(string productId, PurchaseOptions options, CancellationToken ct)
        {
            Task<PurchaseResult> Done(PurchaseResult r) => Task.FromResult(r);
            if (!IsInitialized || m_Context.IsDisposed)
                return Done(new PurchaseResult(PurchaseStatus.NotInitialized, productId, message: "billing is not initialized"));

            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId);
            if (config == null)
                return Done(new PurchaseResult(PurchaseStatus.Failed, productId, message: "unknown product"));
            // Several stores (CompositeStoreAdapter): the router picks this purchase's store.
            var composite = m_Store as CompositeStoreAdapter;
            var store = composite?.StoreFor(config) ?? m_Store.Store;
            var storeId = config.StoreId(store);
            if (SyncUser() == null)
                return Done(new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: "user id is not set"));
            if (m_Waiters.ContainsKey(storeId))
                return Done(new PurchaseResult(PurchaseStatus.AlreadyInProgress, config.id, storeId));

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
                    return Done(new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: problem));
                replacedStoreId = replaced.StoreId(m_Store.Store);
            }

            BindAccount();
            var waiter = StartWaiting(storeId);
            m_Orders.SetPurchaseContext(storeId, options?.Paywall, options?.OfferId);
            void Start()
            {
                if (replacedStoreId != null)
                    ((ISubscriptionChangeStore)m_Store).PurchaseReplacing(storeId, replacedStoreId, options.ReplacementMode);
                else if (composite != null)
                    composite.PurchaseIn(store, storeId);
                else
                    m_Store.Purchase(storeId);
            }
            if (m_Store is ICheckoutStoreAdapter checkout)
                return StartCheckoutAsync(checkout, config, storeId, waiter, Start, ct).Unwrap();
            return StartInStore(config, storeId, waiter, Start, ct);
        }

        /// <summary>A checkout store (web) gets the player and their attribution first.</summary>
        private async Task<Task<PurchaseResult>> StartCheckoutAsync(ICheckoutStoreAdapter checkout, BillingProductConfig config,
            string storeId, PurchaseWaiter waiter, Action start, CancellationToken ct)
        {
            try
            {
                checkout.PrepareCheckout(new CheckoutContext { UserId = m_Context.User, Attribution = await m_Context.AttributionAsync() });
            }
            catch (Exception e)
            {
                return Task.FromResult(StartFailed(config, storeId, e.Message));
            }
            return StartInStore(config, storeId, waiter, start, ct);
        }

        /// <summary>Starts the purchase in the store and returns what the caller awaits: the waiter's task, or the
        /// failure when it could not start.</summary>
        private Task<PurchaseResult> StartInStore(BillingProductConfig config, string storeId, PurchaseWaiter waiter, Action start,
            CancellationToken ct)
        {
            if (m_Context.IsDisposed)
            {
                StartFailed(config, storeId, null);
                return Task.FromResult(new PurchaseResult(PurchaseStatus.NotInitialized, config.id, storeId, message: "billing was disposed"));
            }
            try
            {
                start();
            }
            catch (Exception e)
            {
                return Task.FromResult(StartFailed(config, storeId, e.Message));
            }
            return AwaitResult(storeId, waiter, ct);
        }

        /// <summary>The purchase did not start: nobody waits for it and its paywall and offer are dropped.</summary>
        private PurchaseResult StartFailed(BillingProductConfig config, string storeId, string message)
        {
            m_Waiters.Remove(storeId);
            m_Orders.ClearPurchaseContext(storeId);
            return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: message);
        }

        /// <summary>What the caller of a started purchase awaits: the waiter's own task. Cancelling
        /// <paramref name="ct"/> stops waiting only (on the main thread): the purchase goes on in the store and keeps its
        /// paywall and offer for the server.</summary>
        private Task<PurchaseResult> AwaitResult(string storeId, PurchaseWaiter waiter, CancellationToken ct)
        {
            if (ct.CanBeCanceled && !waiter.Task.IsCompleted)
            {
                var main = SynchronizationContext.Current;
                waiter.Cancellation = ct.Register(() => MainThreadPost.Run(main, () =>
                {
                    if (m_Waiters.TryGetValue(storeId, out var current) && current == waiter)
                        m_Waiters.Remove(storeId);
                    waiter.TrySetCanceled();
                }));
            }
            return waiter.Task;
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
                    waiter.Cancellation.Dispose();
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
                if (handlers != null)
                    return ReportOutcome.Received;
                return received ? ReportOutcome.Awaited : ReportOutcome.NoReceiver;
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
