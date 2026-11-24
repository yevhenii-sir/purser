using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>What happened to a purchase result handed to the game (<see cref="IOrderProcessorHost.Report"/>).</summary>
    internal enum ReportOutcome
    {
        /// <summary>The purchase's waiter (<c>PurchaseAsync</c>) or a handler of its event got it.</summary>
        Received,

        /// <summary>Nobody waited for the purchase and its event has no handler.</summary>
        NoReceiver,

        /// <summary>A handler of its event threw (logged by the host).</summary>
        HandlerFailed,
    }

    /// <summary>What <see cref="OrderProcessor"/> asks of the service that owns it.</summary>
    internal interface IOrderProcessorHost
    {
        /// <summary>Products loaded from the store: the price the store charges goes to <c>/verify</c>.</summary>
        IReadOnlyList<StoreProduct> Products { get; }

        /// <summary>Ends when the product fetch of the initialization attempt that is running ended, either way; null
        /// or completed when none is running.</summary>
        Task ProductsFetch { get; }

        /// <summary>Reads the user id again, handling a changed player first; null when there is no player.</summary>
        string SyncUser();

        /// <summary>Hands the result of <paramref name="order"/> to the game: completes the waiter of the purchase that
        /// brought the order and raises its event. A throwing handler is logged, never passed on.</summary>
        ReportOutcome Report(PurchaseResult result, StoreOrder order);
    }

    /// <summary>
    /// Verification of store orders: an order the store delivers (<see cref="IStoreAdapter.OrderPending"/>) is sent to
    /// the server (<c>/verify/{store}</c>, retried while the server cannot be reached), its result is reported to the
    /// game through <see cref="IOrderProcessorHost.Report"/> and the order is finished in the store
    /// (<see cref="IStoreAdapter.Finish"/>) once the server accepted or definitively rejected it.
    /// Used from the Unity main thread only.
    /// </summary>
    internal sealed partial class OrderProcessor
    {
        internal enum Outcome
        {
            Success,
            PendingDelivery,
            PendingPayment,
            Rejected,
            Retry,
        }

        /// <summary>What the game said about a purchase it started, sent with its <c>/verify</c>.</summary>
        private sealed class PurchaseContext
        {
            public Paywall Paywall;
            public string OfferId;
            /// <summary>The orders known when the purchase started (<see cref="KnownOrders"/>): they are not its order.</summary>
            public HashSet<string> Older;
        }

        /// <summary>An order whose verification was given up for now, with what the game said about its purchase.</summary>
        private sealed class GivenUpOrder
        {
            public StoreOrder Order;
            public PurchaseContext Context;
        }

        /// <summary>A purchase the game must grant itself that nobody received yet.</summary>
        private sealed class HeldResult
        {
            public StoreOrder Order;
            public PurchaseResult Result;
            /// <summary>A handler is being called with it right now.</summary>
            public bool Delivering;
        }

        /// <summary>Rate-limited verification retries (429 with <c>Retry-After</c>) that do not use up
        /// <see cref="BillingConfig.retryDelaysSeconds"/>.</summary>
        internal const int MaxRateLimitedRetries = 10;

        /// <summary>How long an order that arrives while the products are being fetched waits for them (the price the
        /// store charges goes to <c>/verify</c>).</summary>
        internal static readonly TimeSpan ProductsWait = TimeSpan.FromSeconds(10);

        private readonly BillingContext m_Context;
        private readonly IOrderProcessorHost m_Host;
        private readonly CustomerInfoRefresher m_Refresher;
        private readonly BillingConfig m_Config;
        private readonly IStoreAdapter m_Store;
        private readonly IBillingApi m_Api;
        private readonly BillingLog m_Log;
        /// <summary>Where purchases granted before the server answered (fail-open) wait for it.</summary>
        private readonly ObservedPurchases m_Observed;

        /// <summary>Keys (<see cref="StoreOrder.Key"/>) of orders being verified.</summary>
        private readonly HashSet<string> m_InFlightKeys = new HashSet<string>();
        /// <summary>Transaction ids of orders being verified: the same purchase may arrive under another key.</summary>
        private readonly HashSet<string> m_InFlightTransactions = new HashSet<string>();
        /// <summary>The verifications that are waiting for something (the server, a retry delay), by order key.</summary>
        private readonly Dictionary<string, Task> m_InFlightTasks = new Dictionary<string, Task>();
        private readonly Dictionary<string, StoreOrder> m_WaitingForUser = new Dictionary<string, StoreOrder>();
        /// <summary>Orders the server could not be asked about, by key: verified again when the app returns to the
        /// foreground or the store reconnects (<see cref="RetryGivenUp"/>).</summary>
        private readonly Dictionary<string, GivenUpOrder> m_GivenUp = new Dictionary<string, GivenUpOrder>();
        /// <summary>Verified purchases the game grants itself that had no receiver, by key: not finished in the store
        /// until a <c>PurchaseSucceeded</c> handler takes them (<see cref="DeliverHeld"/>).</summary>
        private readonly Dictionary<string, HeldResult> m_Held = new Dictionary<string, HeldResult>();
        /// <summary>Paywall and offer of the purchase started for a store product id (sent with /verify). Kept until the
        /// purchase ends, also when the caller stopped waiting for it.</summary>
        private readonly Dictionary<string, PurchaseContext> m_PurchaseContext = new Dictionary<string, PurchaseContext>();

        /// <param name="refresher">Refreshed after a purchase that changes what the player has; holds the local grants
        /// of the Editor without the server.</param>
        public OrderProcessor(BillingContext context, IOrderProcessorHost host, CustomerInfoRefresher refresher,
            ObservedPurchases observed = null)
        {
            m_Observed = observed;
            m_Context = context;
            m_Host = host;
            m_Refresher = refresher;
            m_Config = context.Config;
            m_Store = context.Store;
            m_Api = context.Api;
            m_Log = context.Log;
        }

        // ------------------------------------------------------------------
        // What the game said about a purchase
        // ------------------------------------------------------------------

        /// <summary>Remembers the paywall and the offer of the purchase the game starts for a store product id; forgets
        /// the previous ones when both are empty. Orders known by now (<see cref="KnownOrders"/>) do not get them.</summary>
        public void SetPurchaseContext(string storeId, Paywall paywall, string offerId)
        {
            if (paywall == null && string.IsNullOrEmpty(offerId))
                m_PurchaseContext.Remove(storeId);
            else
                m_PurchaseContext[storeId] = new PurchaseContext { Paywall = paywall, OfferId = NullIfEmpty(offerId), Older = KnownOrders() };
        }

        /// <summary>The purchase of a store product id ended without an order: its paywall and offer are not sent with
        /// later orders.</summary>
        public void ClearPurchaseContext(string storeId) => m_PurchaseContext.Remove(storeId);

        /// <summary>What the game said about the purchase that brought <paramref name="order"/>; null when it said
        /// nothing, or the order was known before that purchase started.</summary>
        private PurchaseContext ContextOf(StoreOrder order)
        {
            if (string.IsNullOrEmpty(order.StoreProductId) || !m_PurchaseContext.TryGetValue(order.StoreProductId, out var context))
                return null;
            return IsKnown(context.Older, order) ? null : context;
        }

        /// <summary>The order ended (or was given up): later orders of its product do not inherit its paywall and offer.</summary>
        private void ReleaseContext(StoreOrder order, PurchaseContext context)
        {
            if (context != null && !string.IsNullOrEmpty(order.StoreProductId) &&
                m_PurchaseContext.TryGetValue(order.StoreProductId, out var current) && ReferenceEquals(current, context))
                m_PurchaseContext.Remove(order.StoreProductId);
        }

        // ------------------------------------------------------------------
        // Orders
        // ------------------------------------------------------------------

        /// <summary>The order is being verified right now.</summary>
        public bool IsInFlight(StoreOrder order) =>
            m_InFlightKeys.Contains(order.Key) ||
            (!string.IsNullOrEmpty(order.TransactionId) && m_InFlightTransactions.Contains(order.TransactionId));

        /// <summary>The verifications running now, one task per order. A task ends when its order was reported, given
        /// up for now or the service was disposed; it does not fault.</summary>
        public IReadOnlyCollection<Task> InFlightTasks => m_InFlightTasks.Values;

        /// <summary>
        /// Keys and transaction ids of every order the processor has now: being verified, waiting for a player, given
        /// up, held for a handler. A purchase the game starts after this cannot have brought any of them
        /// (<see cref="IsKnown"/>).
        /// </summary>
        public HashSet<string> KnownOrders()
        {
            var known = new HashSet<string>(m_InFlightKeys);
            known.UnionWith(m_InFlightTransactions);
            foreach (var order in m_WaitingForUser.Values.Concat(m_GivenUp.Values.Select(g => g.Order)).Concat(m_Held.Values.Select(h => h.Order)))
            {
                known.Add(order.Key);
                if (!string.IsNullOrEmpty(order.TransactionId))
                    known.Add(order.TransactionId);
            }
            return known;
        }

        /// <summary><paramref name="order"/> is one of <paramref name="known"/> (<see cref="KnownOrders"/>), by its key
        /// or its transaction id.</summary>
        public static bool IsKnown(HashSet<string> known, StoreOrder order) =>
            known != null && order != null &&
            ((!string.IsNullOrEmpty(order.Key) && known.Contains(order.Key)) ||
             (!string.IsNullOrEmpty(order.TransactionId) && known.Contains(order.TransactionId)));

        /// <summary>The store delivered an order (<see cref="IStoreAdapter.OrderPending"/>): verifies it, or keeps it
        /// until there is a player.</summary>
        public void OnOrderPending(StoreOrder order)
        {
            if (m_Context.IsDisposed)
                return;
            var key = order.Key;
            if (string.IsNullOrEmpty(key))
            {
                m_Log.Error($"Order {order} has no transaction id; skipped.");
                return;
            }
            if (IsInFlight(order))
                return;
            var held = FindHeld(order);
            if (held != null)
            {
                // Verified already; it waits for a PurchaseSucceeded handler. Finish what the store delivered last.
                held.Order = order;
                return;
            }
            if (m_Host.SyncUser() == null)
            {
                m_WaitingForUser[key] = order;
                if (m_Log.IsEnabled(BillingLogLevel.Info))
                    m_Log.Info($"Order {order} waits for a user id.");
                return;
            }
            var verification = ProcessOrderAsync(order, TakeGivenUp(order));
            // Kept while it waits; one that needed no waiting (the Editor without the server) is done already.
            if (!verification.IsCompleted)
                m_InFlightTasks[key] = verification;
        }

        /// <summary>Verifies the orders that waited for a player, once there is one.</summary>
        public void ProcessWaitingOrders()
        {
            if (m_Context.User == null || m_WaitingForUser.Count == 0)
                return;
            var orders = m_WaitingForUser.Values.ToList();
            m_WaitingForUser.Clear();
            foreach (var order in orders)
                OnOrderPending(order);
        }

        /// <summary>Verifies again the orders that were given up (the server could not be reached): the app returned
        /// to the foreground or the store reconnected.</summary>
        public void RetryGivenUp()
        {
            if (m_Context.IsDisposed || m_Context.User == null || m_GivenUp.Count == 0)
                return;
            foreach (var givenUp in m_GivenUp.Values.ToList())
                OnOrderPending(givenUp.Order);
        }

        /// <summary>
        /// Waits until the verifications running now ended, at most <paramref name="timeout"/> (one that waits to retry
        /// takes longer).
        /// </summary>
        public async Task WaitForInFlightAsync(TimeSpan timeout, CancellationToken ct)
        {
            var running = m_InFlightTasks.Values.ToList();
            if (running.Count == 0)
                return;
            using (var cts = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                await Task.WhenAny(Task.WhenAll(running), m_Context.Delay(timeout, cts.Token));
                cts.Cancel();
            }
            ct.ThrowIfCancellationRequested();
        }

        /// <summary>
        /// A handler was added to <c>PurchaseSucceeded</c>: gives it the purchases that had no receiver, and finishes
        /// each one it took in the store. One it throws for stays held.
        /// </summary>
        public void DeliverHeld(Action<PurchaseResult> handler)
        {
            if (m_Context.IsDisposed || handler == null || m_Held.Count == 0)
                return;
            foreach (var entry in m_Held.ToList())
            {
                var held = entry.Value;
                // A handler that adds another handler, or the store delivering the order again, gets here again.
                if (held.Delivering || !m_Held.TryGetValue(entry.Key, out var current) || !ReferenceEquals(current, held))
                    continue;
                held.Delivering = true;
                try
                {
                    if (m_Log.IsEnabled(BillingLogLevel.Info))
                        m_Log.Info("Purchase " + held.Result + " (kept until a PurchaseSucceeded handler was added)");
                    handler(held.Result);
                }
                catch (Exception e)
                {
                    m_Log.Error($"PurchaseSucceeded handler threw for {held.Result}: {e}");
                    continue;
                }
                finally
                {
                    held.Delivering = false;
                }
                m_Held.Remove(entry.Key);
                m_Store.Finish(held.Order, true);
            }
        }

        /// <summary>The store confirmed (or refused to confirm) an order (<see cref="IStoreAdapter.OrderFinished"/>).</summary>
        public void OnOrderFinished(StoreOrder order, bool success, string error)
        {
            if (!success && m_Log.IsEnabled(BillingLogLevel.Verbose))
                m_Log.Verbose($"Store confirmation of {order} reported: {error} " +
                              "(expected when the server already consumed the purchase).");
        }

        private HeldResult FindHeld(StoreOrder order)
        {
            if (m_Held.Count == 0)
                return null;
            if (m_Held.TryGetValue(order.Key, out var held))
                return held;
            return string.IsNullOrEmpty(order.TransactionId)
                ? null
                : m_Held.Values.FirstOrDefault(h => h.Order.TransactionId == order.TransactionId);
        }

        /// <summary>The given-up entry of the order, removed; null when it was not given up.</summary>
        private GivenUpOrder TakeGivenUp(StoreOrder order)
        {
            if (m_GivenUp.Count == 0)
                return null;
            var key = order.Key;
            if (!m_GivenUp.ContainsKey(key))
                key = string.IsNullOrEmpty(order.TransactionId)
                    ? null
                    : m_GivenUp.FirstOrDefault(kv => kv.Value.Order.TransactionId == order.TransactionId).Key;
            if (key == null)
                return null;
            var givenUp = m_GivenUp[key];
            m_GivenUp.Remove(key);
            return givenUp;
        }

        /// <summary>Keeps the order for <see cref="RetryGivenUp"/> with what the game said about its purchase, which
        /// later orders of the product must not inherit.</summary>
        private void GiveUp(StoreOrder order, PurchaseContext context)
        {
            m_GivenUp[order.Key] = new GivenUpOrder { Order = order, Context = context };
            ReleaseContext(order, context);
        }
    }
}
