using System;
using System.Collections.Generic;
using System.Linq;
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

        /// <summary>Reads the user id again, handling a changed player first; null when there is no player.</summary>
        string SyncUser();

        /// <summary>Hands a purchase result to the game: completes the purchase's waiter and raises its event.
        /// A throwing handler is logged, never passed on.</summary>
        ReportOutcome Report(PurchaseResult result);
    }

    /// <summary>
    /// Verification of store orders: an order the store delivers (<see cref="IStoreAdapter.OrderPending"/>) is sent to
    /// the server (<c>/verify/{store}</c>, retried while the server cannot be reached), its result is reported to the
    /// game through <see cref="IOrderProcessorHost.Report"/> and the order is finished in the store
    /// (<see cref="IStoreAdapter.Finish"/>) once the server accepted or definitively rejected it.
    /// Used from the Unity main thread only.
    /// </summary>
    internal sealed class OrderProcessor
    {
        private enum Outcome
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
        }

        /// <summary>Rate-limited verification retries (429 with <c>Retry-After</c>) that do not use up
        /// <see cref="BillingConfig.retryDelaysSeconds"/>.</summary>
        internal const int MaxRateLimitedRetries = 10;

        private readonly BillingContext m_Context;
        private readonly IOrderProcessorHost m_Host;
        private readonly CustomerInfoRefresher m_Refresher;
        private readonly BillingConfig m_Config;
        private readonly IStoreAdapter m_Store;
        private readonly IBillingApi m_Api;
        private readonly BillingLog m_Log;

        /// <summary>Keys (<see cref="StoreOrder.Key"/>) of orders being verified.</summary>
        private readonly HashSet<string> m_InFlightKeys = new HashSet<string>();
        /// <summary>Transaction ids of orders being verified: the same purchase may arrive under another key.</summary>
        private readonly HashSet<string> m_InFlightTransactions = new HashSet<string>();
        /// <summary>The verifications that are waiting for something (the server, a retry delay), by order key.</summary>
        private readonly Dictionary<string, Task> m_InFlightTasks = new Dictionary<string, Task>();
        private readonly Dictionary<string, StoreOrder> m_WaitingForUser = new Dictionary<string, StoreOrder>();
        /// <summary>Paywall and offer of the purchase started for a store product id (sent with /verify). Kept until the
        /// purchase ends, also when the caller stopped waiting for it.</summary>
        private readonly Dictionary<string, PurchaseContext> m_PurchaseContext = new Dictionary<string, PurchaseContext>();

        /// <param name="refresher">Refreshed after a purchase that changes what the player has; holds the local grants
        /// of the Editor without the server.</param>
        public OrderProcessor(BillingContext context, IOrderProcessorHost host, CustomerInfoRefresher refresher)
        {
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
        /// the previous ones when both are empty.</summary>
        public void SetPurchaseContext(string storeId, Paywall paywall, string offerId)
        {
            if (paywall == null && string.IsNullOrEmpty(offerId))
                m_PurchaseContext.Remove(storeId);
            else
                m_PurchaseContext[storeId] = new PurchaseContext { Paywall = paywall, OfferId = NullIfEmpty(offerId) };
        }

        /// <summary>The purchase of a store product id ended: its paywall and offer are not sent with later orders.</summary>
        public void ClearPurchaseContext(string storeId) => m_PurchaseContext.Remove(storeId);

        // ------------------------------------------------------------------
        // Orders
        // ------------------------------------------------------------------

        /// <summary>The order is being verified right now.</summary>
        public bool IsInFlight(StoreOrder order) =>
            m_InFlightKeys.Contains(order.Key) ||
            (!string.IsNullOrEmpty(order.TransactionId) && m_InFlightTransactions.Contains(order.TransactionId));

        /// <summary>The verifications running now, one task per order. A task ends when its order was reported, given
        /// up for the session or the service was disposed; it does not fault.</summary>
        public IReadOnlyCollection<Task> InFlightTasks => m_InFlightTasks.Values;

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
            if (m_Host.SyncUser() == null)
            {
                m_WaitingForUser[key] = order;
                m_Log.Info($"Order {order} waits for a user id.");
                return;
            }
            var verification = ProcessOrderAsync(order);
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

        /// <summary>The store confirmed (or refused to confirm) an order (<see cref="IStoreAdapter.OrderFinished"/>).</summary>
        public void OnOrderFinished(StoreOrder order, bool success, string error)
        {
            if (!success)
                m_Log.Verbose($"Store confirmation of {order} reported: {error} " +
                              "(expected when the server already consumed the purchase).");
        }

        private async Task ProcessOrderAsync(StoreOrder order)
        {
            var key = order.Key;
            var transactionId = string.IsNullOrEmpty(order.TransactionId) ? null : order.TransactionId;
            m_InFlightKeys.Add(key);
            if (transactionId != null)
                m_InFlightTransactions.Add(transactionId);
            var config = m_Config.FindByStoreId(order.StoreProductId);
            var productId = config?.id ?? order.StoreProductId;
            try
            {
                var user = m_Context.User;
                if (m_Context.SkipServer)
                {
                    m_Store.Finish(order, true);
                    if (config != null)
                        m_Refresher.GrantLocally(user, config);
                    m_Host.Report(new PurchaseResult(PurchaseStatus.Success, productId, order.StoreProductId,
                        order.TransactionId ?? key, "editor: confirmed locally without the server"));
                    return;
                }

                var request = await BuildVerifyRequestAsync(order, user);
                if (m_Context.IsDisposed)
                    return;
                var delays = m_Config.retryDelaysSeconds ?? new int[0];
                var attempt = 0;
                var rateLimited = 0;
                var reported = false;
                while (true)
                {
                    // Webhook stores (Stripe, Xsolla) have no receipt: the adapter asks the server about the order.
                    var response = m_Store is IWebhookStoreAdapter webhookStore
                        ? await webhookStore.CheckOrderAsync(order, request, m_Context.Lifetime)
                        : await m_Api.VerifyAsync(m_Store.Store, request, m_Context.Lifetime);
                    if (m_Context.IsDisposed)
                        return;
                    var outcome = Classify(response);
                    if (outcome != Outcome.Retry)
                    {
                        if (config == null && !string.IsNullOrEmpty(response.Value?.ProductId))
                        {
                            // The store did not name the product (a Steam order authorized before a restart, a resumed
                            // Stripe session): the server's answer does.
                            config = m_Config.FindByStoreId(response.Value.ProductId);
                            productId = config?.id ?? response.Value.ProductId;
                        }
                        Apply(order, config, productId, response.Value, outcome);
                        return;
                    }

                    m_Log.Warning($"Verification of {order} failed ({response}), attempt {attempt + rateLimited + 1}.");
                    if (!reported)
                    {
                        reported = true;
                        m_Host.Report(new PurchaseResult(PurchaseStatus.PendingVerification, productId, order.StoreProductId,
                            order.TransactionId, "payment received, verification will be retried", response?.Value));
                    }
                    TimeSpan wait;
                    if (response != null && response.IsRateLimited && response.RetryAfter.HasValue && rateLimited < MaxRateLimitedRetries)
                    {
                        // The server asked to wait (429): it is not a failed attempt.
                        rateLimited++;
                        wait = response.RetryAfter.Value;
                    }
                    else
                    {
                        if (attempt >= delays.Length)
                        {
                            m_Log.Warning($"Giving up on {order} for this session; the store will deliver it again on next launch.");
                            return;
                        }
                        wait = TimeSpan.FromSeconds(delays[attempt]);
                        attempt++;
                    }
                    await m_Context.Delay(wait, m_Context.Lifetime);
                    if (m_Context.IsDisposed)
                        return;
                }
            }
            catch (OperationCanceledException)
            {
                // Service disposed.
            }
            catch (ObjectDisposedException) when (m_Context.IsDisposed)
            {
            }
            catch (Exception e)
            {
                m_Log.Error($"Processing {order} failed: {e}");
                m_Host.Report(new PurchaseResult(PurchaseStatus.PendingVerification, productId, order.StoreProductId,
                    order.TransactionId, e.Message));
            }
            finally
            {
                m_InFlightKeys.Remove(key);
                if (transactionId != null)
                    m_InFlightTransactions.Remove(transactionId);
                m_InFlightTasks.Remove(key);
            }
        }

        private async Task<VerifyRequest> BuildVerifyRequestAsync(StoreOrder order, string user)
        {
            // A fallback price is the game's guess, not what the store charged: the server is not told it.
            var product = m_Host.Products.FirstOrDefault(p => p.StoreId == order.StoreProductId && !p.IsFallbackPrice);
            // Read before the first await: the purchase that set it may finish meanwhile.
            m_PurchaseContext.TryGetValue(order.StoreProductId, out var context);
            var paywall = context?.Paywall;
            return new VerifyRequest
            {
                AppId = m_Context.AppId,
                UserId = user,
                ProductId = order.StoreProductId,
                PurchaseToken = NullIfEmpty(order.PurchaseToken),
                OrderId = string.IsNullOrEmpty(order.TransactionId) ? null : order.TransactionId,
                StoreUserId = string.IsNullOrEmpty(order.StoreUserId) ? null : order.StoreUserId,
                AppInstanceId = await m_Context.AppInstanceIdAsync(),
                PriceMicros = product?.PriceMicros ?? 0,
                Currency = string.IsNullOrEmpty(product?.Currency) ? null : product.Currency,
                Attribution = await m_Context.AttributionAsync(),
                Placement = NullIfEmpty(paywall?.PlacementId),
                VariantId = NullIfEmpty(paywall?.VariantId),
                TestId = NullIfEmpty(paywall?.TestId),
                PaywallId = NullIfEmpty(paywall?.PaywallId),
                OfferId = context?.OfferId,
                Client = m_Context.Client,
            };
        }

        private static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;

        private static Outcome Classify(ApiResult<VerifyResponse> response)
        {
            if (response == null || response.IsTransportError || response.IsRateLimited)
                return Outcome.Retry;
            switch (response.Value?.Status)
            {
                case VerifyStatus.Success:
                    return Outcome.Success;
                case VerifyStatus.Pending:
                    return Outcome.PendingDelivery;
                case VerifyStatus.PendingOrCanceled:
                    return Outcome.PendingPayment;
                case VerifyStatus.Invalid:
                case VerifyStatus.Forbidden:
                case VerifyStatus.Expired:
                case VerifyStatus.Revoked:
                    return Outcome.Rejected;
                default:
                    // `error` (server misconfiguration, store outage, refused player token), 5xx, unexpected body: keep
                    // the order and retry.
                    return Outcome.Retry;
            }
        }

        private void Apply(StoreOrder order, BillingProductConfig config, string productId, VerifyResponse server, Outcome outcome)
        {
            var transactionId = server?.TransactionId ?? order.TransactionId;
            switch (outcome)
            {
                case Outcome.Success:
                case Outcome.PendingDelivery:
                    var result = new PurchaseResult(
                        outcome == Outcome.Success ? PurchaseStatus.Success : PurchaseStatus.PendingDelivery,
                        productId, order.StoreProductId, transactionId, server?.Message, server);
                    if (result.GrantOnClient)
                    {
                        // The game grants the reward in PurchaseSucceeded. Finish the purchase (Google
                        // consume / Apple finishTransaction) only after that: if the app dies in between,
                        // the store delivers it again and the server answers success from its ledger.
                        if (m_Host.Report(result) == ReportOutcome.HandlerFailed)
                        {
                            m_Log.Error($"PurchaseSucceeded handler failed for {order}, not finishing it " +
                                        "so the store delivers it again.");
                            return;
                        }
                        m_Store.Finish(order, true);
                    }
                    else
                    {
                        m_Store.Finish(order, true);
                        m_Host.Report(result);
                    }
                    if (config == null || config.type != BillingProductType.Consumable)
                        m_Refresher.RefreshInBackground();
                    break;
                case Outcome.PendingPayment:
                    m_Host.Report(new PurchaseResult(PurchaseStatus.PendingPayment, productId, order.StoreProductId,
                        transactionId, "payment is not completed yet", server));
                    break;
                case Outcome.Rejected:
                    // Definitive answer: finish the transaction so the store stops re-delivering it.
                    m_Store.Finish(order, false);
                    m_Host.Report(new PurchaseResult(PurchaseStatus.Rejected, productId, order.StoreProductId,
                        transactionId, $"{server?.Status}: {server?.Message}", server));
                    break;
            }
        }
    }
}
