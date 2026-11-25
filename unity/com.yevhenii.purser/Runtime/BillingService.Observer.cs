using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Observer mode: purchases of the game's own store code (ReportPurchaseAsync, ReportRestoredAsync), and the queue of
    // purchases finished before the server answered (ObservedPurchases), which also keeps fail-open grants.
    public sealed partial class BillingService : IObservedPurchasesHost
    {
        /// <inheritdoc/>
        public event Action<PurchaseResult> PurchaseVerified;

        /// <inheritdoc/>
        public event Action<PurchaseResult> PurchaseRejected;

        /// <inheritdoc/>
        public async Task<PurchaseResult> ReportPurchaseAsync(ExternalPurchase purchase, ReportMode mode = ReportMode.RecordOnly,
            CancellationToken ct = default)
        {
            ThrowIfDisposed();
            if (purchase == null)
                throw new ArgumentNullException(nameof(purchase));
            var productId = m_Config.FindByStoreId(purchase.StoreProductId)?.id ?? purchase.StoreProductId;
            var problem = purchase.Problem();
            if (problem != null)
            {
                m_Log.Error($"ReportPurchaseAsync: {problem} ({purchase})");
                return new PurchaseResult(PurchaseStatus.Failed, productId, purchase.StoreProductId, purchase.TransactionId, problem);
            }
            // Kept before anything else: the game finishes the purchase in the store right away, so the store never
            // delivers it again.
            m_Observed.Add(purchase, provisional: false);
            if (SyncUser() == null)
                return new PurchaseResult(PurchaseStatus.PendingVerification, productId, purchase.StoreProductId, purchase.TransactionId,
                    "kept until there is a player id (SetUserId)");

            var verification = m_Observed.VerifyAsync(purchase.Key);
            if (mode == ReportMode.RecordOnly)
                return await WithCancellation(verification, ct);

            var config = m_Config.FindByStoreId(purchase.StoreProductId);
            var failOpen = m_Config.failOpenAfterSeconds > 0 && !(config?.strictVerification ?? false);
            if (!failOpen)
                return await WithCancellation(verification, ct);
            using (var cts = CancellationTokenSource.CreateLinkedTokenSource(ct, m_Context.Lifetime))
            {
                var first = await Task.WhenAny(verification, m_Context.Delay(TimeSpan.FromSeconds(m_Config.failOpenAfterSeconds), cts.Token));
                cts.Cancel();
                ct.ThrowIfCancellationRequested();
                if (first == verification)
                {
                    var answered = verification.Result;
                    // Retries ran out before the deadline: the same as no answer in time.
                    if (answered.Status != PurchaseStatus.PendingVerification)
                        return answered;
                }
            }
            m_Observed.MarkProvisional(purchase.Key);
            if (config != null)
                m_Refresher.GrantLocally(m_Context.User, config);
            m_Log.Warning($"The server did not answer about {purchase} within {m_Config.failOpenAfterSeconds} s: provisional success, verified later.");
            return new PurchaseResult(PurchaseStatus.Success, productId, purchase.StoreProductId, purchase.TransactionId,
                "granted before the server answered: verified later", provisional: true);
        }

        /// <inheritdoc/>
        public async Task<CustomerInfo> ReportRestoredAsync(IEnumerable<ExternalPurchase> purchases, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            var items = (purchases ?? Enumerable.Empty<ExternalPurchase>()).Where(p => p != null && p.Problem() == null && p.Store != "fake").ToList();
            var user = SyncUser();
            if (user == null || items.Count == 0 || SkipServer)
                return CustomerInfo;
            // /restore takes one store per call (the build's app id) and a limited number of purchases.
            foreach (var group in items.GroupBy(p => p.Store))
            {
                foreach (var chunk in Chunks(group.ToList(), RestoreRequest.MaxItems))
                {
                    var request = new RestoreRequest
                    {
                        AppId = m_Config.AppIdFor(group.Key),
                        UserId = user,
                        AppInstanceId = await m_Context.AppInstanceIdAsync(),
                        Attribution = await m_Context.AttributionAsync(),
                        Client = m_Client,
                        ClientFinished = true,
                    };
                    foreach (var p in chunk)
                        request.Purchases.Add(new RestoreItem
                        {
                            ProductId = p.StoreProductId,
                            PurchaseToken = NullIfEmpty(p.PurchaseToken),
                            OrderId = NullIfEmpty(p.TransactionId),
                            StoreUserId = NullIfEmpty(p.StoreUserId),
                        });
                    var response = await m_Api.RestoreAsync(request, ct);
                    if (response == null || !response.IsHttpSuccess || response.Value == null)
                    {
                        m_Log.Warning($"Reporting restored purchases failed: {response}");
                    }
                }
            }
            return await RefreshCustomerInfoAsync(ct);
        }

        /// <inheritdoc/>
        public async Task<XsollaClaimResponse> ClaimXsollaPurchasesAsync(string xsollaLoginToken, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            var xsolla = m_Store as XsollaStoreAdapter ?? (m_Store as CompositeStoreAdapter)?.Find(BillingStores.Xsolla) as XsollaStoreAdapter;
            if (xsolla == null)
                throw new BillingException("ClaimXsollaPurchasesAsync needs an XsollaStoreAdapter (BillingDependencies.Store or a CompositeStoreAdapter)");
            var user = SyncUser() ?? throw new BillingException("ClaimXsollaPurchasesAsync needs a player id (SetUserId)");
            var claimed = await xsolla.ClaimInventoryAsync(user, xsollaLoginToken, ct);
            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info($"Moved {claimed.Claimed.Count} Xsolla inventory item(s) to {user}; skipped {claimed.Skipped.Count}.");
            if (claimed.Claimed.Count > 0)
                await RefreshCustomerInfoAsync(ct);
            return claimed;
        }

        private static IEnumerable<List<T>> Chunks<T>(List<T> list, int size)
        {
            for (var i = 0; i < list.Count; i += size)
                yield return list.GetRange(i, Math.Min(size, list.Count - i));
        }

        private static async Task<PurchaseResult> WithCancellation(Task<PurchaseResult> task, CancellationToken ct)
        {
            if (!ct.CanBeCanceled || task.IsCompleted)
                return await task;
            var cancelled = new TaskCompletionSource<PurchaseResult>();
            using (ct.Register(() => cancelled.TrySetCanceled()))
                return await await Task.WhenAny(task, cancelled.Task);
        }

        // ---- IObservedPurchasesHost ----

        string IObservedPurchasesHost.SyncUser() => SyncUser();

        void IObservedPurchasesHost.OnVerified(PurchaseResult result) => Raise(PurchaseVerified, nameof(PurchaseVerified), result);

        void IObservedPurchasesHost.OnRejected(PurchaseResult result)
        {
            // A provisional success nobody received yet must not reach a handler added later.
            m_Orders.DropHeld(result.TransactionId);
            Raise(PurchaseRejected, nameof(PurchaseRejected), result);
        }

        private void Raise(Action<PurchaseResult> handlers, string name, PurchaseResult result)
        {
            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info($"{name}: {result}");
            try
            {
                handlers?.Invoke(result);
            }
            catch (Exception e)
            {
                m_Log.Error($"{name} handler threw for {result}: {e}");
            }
        }
    }
}
