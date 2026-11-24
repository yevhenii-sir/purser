using System;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Verification of one order: the /verify request, its retries and what its answer does.
    internal sealed partial class OrderProcessor
    {
        /// <param name="givenUp">Set when the order was given up before: it keeps what the game said about its
        /// purchase then.</param>
        private async Task ProcessOrderAsync(StoreOrder order, GivenUpOrder givenUp)
        {
            var key = order.Key;
            var transactionId = string.IsNullOrEmpty(order.TransactionId) ? null : order.TransactionId;
            m_InFlightKeys.Add(key);
            if (transactionId != null)
                m_InFlightTransactions.Add(transactionId);
            var config = m_Config.FindByStoreId(order.StoreProductId);
            var productId = config?.id ?? order.StoreProductId;
            // Read before the first await: the purchase that set it may finish meanwhile.
            var context = givenUp != null ? givenUp.Context : ContextOf(order);
            try
            {
                var user = m_Context.User;
                if (m_Context.SkipServer)
                {
                    m_Store.Finish(order, true);
                    if (config != null)
                        m_Refresher.GrantLocally(user, config);
                    ReleaseContext(order, context);
                    m_Host.Report(new PurchaseResult(PurchaseStatus.Success, productId, order.StoreProductId,
                        order.TransactionId ?? key, "editor: confirmed locally without the server"), order);
                    return;
                }

                await WaitForProductsAsync();
                if (m_Context.IsDisposed)
                    return;
                // With several stores (CompositeStoreAdapter) the order goes to its own store's route and build.
                var owner = (m_Store as CompositeStoreAdapter)?.OwnerOf(order) ?? m_Store;
                var request = await BuildVerifyRequestAsync(order, user, context);
                if (m_Context.IsDisposed)
                    return;
                request.AppId = m_Config.AppIdFor(owner.Store);
                var delays = m_Config.retryDelaysSeconds ?? new int[0];
                var attempt = 0;
                var rateLimited = 0;
                var reported = false;
                var failOpenAt = CanFailOpen(owner, config) ? m_Context.UtcNow() + TimeSpan.FromSeconds(m_Config.failOpenAfterSeconds) : (DateTime?)null;
                while (true)
                {
                    // Webhook stores (Stripe, Xsolla) have no receipt: the adapter asks the server about the order.
                    var call = owner is IWebhookStoreAdapter webhookStore
                        ? webhookStore.CheckOrderAsync(order, request, m_Context.Lifetime)
                        : m_Api.VerifyAsync(owner.Store, request, m_Context.Lifetime);
                    if (failOpenAt.HasValue && !await AnsweredBeforeAsync(call, failOpenAt.Value))
                    {
                        // The server did not answer in time: grant now, verify later from the kept purchases.
                        if (m_Context.IsDisposed)
                            return;
                        FailOpen(owner, order, config, productId, user, request, context);
                        return;
                    }
                    var response = await call;
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
                        Apply(order, config, productId, response.Value, outcome, context);
                        return;
                    }

                    m_Log.Warning($"Verification of {order} failed ({response}), attempt {attempt + rateLimited + 1}.");
                    if (!reported)
                    {
                        reported = true;
                        m_Host.Report(new PurchaseResult(PurchaseStatus.PendingVerification, productId, order.StoreProductId,
                            order.TransactionId, "payment received, verification will be retried", response?.Value), order);
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
                            m_Log.Warning($"Giving up on {order} for now; it is verified again when the app returns to the " +
                                          "foreground, the store reconnects or on the next launch.");
                            GiveUp(order, context);
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
                GiveUp(order, context);
                m_Host.Report(new PurchaseResult(PurchaseStatus.PendingVerification, productId, order.StoreProductId,
                    order.TransactionId, e.Message), order);
            }
            finally
            {
                m_InFlightKeys.Remove(key);
                if (transactionId != null)
                    m_InFlightTransactions.Remove(transactionId);
                m_InFlightTasks.Remove(key);
            }
        }

        /// <summary>Fail-open applies to the order: <see cref="BillingConfig.failOpenAfterSeconds"/> is set, the product is
        /// known and not <see cref="BillingProductConfig.strictVerification"/>, and the store's purchase can be verified
        /// after it was finished (receipt stores, not webhook stores).</summary>
        private bool CanFailOpen(IStoreAdapter owner, BillingProductConfig config) =>
            m_Observed != null && m_Config.failOpenAfterSeconds > 0 && config != null && !config.strictVerification &&
            !(owner is IWebhookStoreAdapter) &&
            (owner.Store == BillingStores.Google || owner.Store == BillingStores.Apple || owner.Store == BillingStores.Amazon);

        /// <summary>Waits for <paramref name="call"/> until <paramref name="deadline"/>; false when it did not end by then.</summary>
        private async Task<bool> AnsweredBeforeAsync(Task call, DateTime deadline)
        {
            var left = deadline - m_Context.UtcNow();
            if (call.IsCompleted)
                return true;
            if (left <= TimeSpan.Zero)
                return false;
            using (var cts = CancellationTokenSource.CreateLinkedTokenSource(m_Context.Lifetime))
            {
                var first = await Task.WhenAny(call, m_Context.Delay(left, cts.Token));
                cts.Cancel();
                return first == call;
            }
        }

        /// <summary>
        /// Grants a purchase the server could not be asked about in time (<see cref="BillingConfig.failOpenAfterSeconds"/>):
        /// a provisional success for the game, the order finished in the store like after a success, and the purchase kept
        /// in <see cref="ObservedPurchases"/>, which verifies it later (with <c>client_finished</c>) and raises
        /// <see cref="IBillingService.PurchaseRejected"/> if the server refuses it.
        /// </summary>
        private void FailOpen(IStoreAdapter owner, StoreOrder order, BillingProductConfig config, string productId, string user,
            VerifyRequest request, PurchaseContext context)
        {
            var purchase = new ExternalPurchase
            {
                Store = owner.Store,
                StoreProductId = order.StoreProductId,
                TransactionId = order.TransactionId,
                PurchaseToken = order.PurchaseToken,
                StoreUserId = order.StoreUserId,
                Price = request.PriceMicros / 1_000_000m,
                Currency = request.Currency,
            };
            m_Log.Warning($"The server did not answer about {order} within {m_Config.failOpenAfterSeconds} s: granted provisionally, " +
                          "verified later.");
            m_Observed.Add(purchase, provisional: true);
            ReleaseContext(order, context);
            m_Refresher.GrantLocally(user, config);
            var result = new PurchaseResult(PurchaseStatus.Success, productId, order.StoreProductId, order.TransactionId,
                "granted before the server answered: verified later", provisional: true);
            switch (m_Host.Report(result, order))
            {
                case ReportOutcome.HandlerFailed:
                    // Not finished: the store delivers it again and the game gets another chance.
                    m_Log.Error($"PurchaseSucceeded handler failed for {order}, not finishing it so the store delivers it again.");
                    break;
                case ReportOutcome.NoReceiver:
                    m_Held[order.Key] = new HeldResult { Order = order, Result = result };
                    break;
                default:
                    m_Store.Finish(order, true);
                    break;
            }
            _ = m_Observed.VerifyAsync(purchase.Key);
        }

        /// <summary>An order that arrives while the products are being fetched (the store delivers unfinished purchases
        /// right after it connects) waits for them, at most <see cref="ProductsWait"/>: its price goes to <c>/verify</c>.</summary>
        private async Task WaitForProductsAsync()
        {
            var fetch = m_Host.ProductsFetch;
            if (fetch == null || fetch.IsCompleted)
                return;
            using (var cts = CancellationTokenSource.CreateLinkedTokenSource(m_Context.Lifetime))
            {
                await Task.WhenAny(fetch, m_Context.Delay(ProductsWait, cts.Token));
                cts.Cancel();
            }
        }

        private async Task<VerifyRequest> BuildVerifyRequestAsync(StoreOrder order, string user, PurchaseContext context)
        {
            // A fallback price is the game's guess, not what the store charged: the server is not told it.
            var product = m_Host.Products.FirstOrDefault(p => p.StoreId == order.StoreProductId && !p.IsFallbackPrice);
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

        internal static Outcome Classify(ApiResult<VerifyResponse> response)
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

        private void Apply(StoreOrder order, BillingProductConfig config, string productId, VerifyResponse server, Outcome outcome,
            PurchaseContext context)
        {
            var transactionId = server?.TransactionId ?? order.TransactionId;
            switch (outcome)
            {
                case Outcome.Success:
                case Outcome.PendingDelivery:
                    ReleaseContext(order, context);
                    var result = new PurchaseResult(
                        outcome == Outcome.Success ? PurchaseStatus.Success : PurchaseStatus.PendingDelivery,
                        productId, order.StoreProductId, transactionId, server?.Message, server);
                    if (result.GrantOnClient)
                    {
                        // The game grants the reward in PurchaseSucceeded. Finish the purchase (Google
                        // consume / Apple finishTransaction) only after that: if the app dies in between,
                        // the store delivers it again and the server answers success from its ledger.
                        switch (m_Host.Report(result, order))
                        {
                            case ReportOutcome.HandlerFailed:
                                m_Log.Error($"PurchaseSucceeded handler failed for {order}, not finishing it " +
                                            "so the store delivers it again.");
                                return;
                            case ReportOutcome.NoReceiver:
                                // Nobody could grant it: kept for the first PurchaseSucceeded handler (DeliverHeld). If
                                // the app ends first, the store delivers it again on the next launch.
                                m_Log.Warning($"Nobody received {order} (no PurchaseSucceeded handler and no PurchaseAsync " +
                                              "waiting): it is not finished in the store until a handler is added.");
                                m_Held[order.Key] = new HeldResult { Order = order, Result = result };
                                break;
                            default:
                                m_Store.Finish(order, true);
                                break;
                        }
                    }
                    else
                    {
                        m_Store.Finish(order, true);
                        m_Host.Report(result, order);
                    }
                    if (config == null || config.type != BillingProductType.Consumable)
                        m_Refresher.RefreshInBackground();
                    break;
                case Outcome.PendingPayment:
                    m_Host.Report(new PurchaseResult(PurchaseStatus.PendingPayment, productId, order.StoreProductId,
                        transactionId, "payment is not completed yet", server), order);
                    break;
                case Outcome.Rejected:
                    // Definitive answer: finish the transaction so the store stops re-delivering it.
                    ReleaseContext(order, context);
                    m_Store.Finish(order, false);
                    m_Host.Report(new PurchaseResult(PurchaseStatus.Rejected, productId, order.StoreProductId,
                        transactionId, $"{server?.Status}: {server?.Message}", server), order);
                    break;
            }
        }
    }
}
