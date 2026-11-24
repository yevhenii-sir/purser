using System;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Customer info & restore. The customer info itself is kept and refreshed by CustomerInfoRefresher, which calls
    // back through ICustomerInfoHost.
    public sealed partial class BillingService : ICustomerInfoHost
    {
        /// <inheritdoc/>
        /// <exception cref="ObjectDisposedException">The service was disposed.</exception>
        /// <exception cref="BillingException">Billing is not initialized, or the user id is not set.</exception>
        public async Task<CustomerInfo> RestoreAsync(CancellationToken ct = default)
        {
            ThrowIfDisposed();
            if (!IsInitialized)
                throw new BillingException("Billing is not initialized");
            var user = SyncUser(refreshOnChange: false) ?? throw new BillingException("User id is not set");
            if (SkipServer)
                return m_Refresher.Current;

            if (Platform == BillingPlatform.Apple)
                await m_Store.RestoreTransactionsAsync(ct);

            // Orders being verified right now (e.g. re-delivered by RestoreTransactions) are not sent again.
            var orders = await m_Store.FetchPurchasesAsync(ct);
            ThrowIfDisposed();
            var items = orders
                .Where(o => !string.IsNullOrEmpty(o.Key) && !m_Orders.IsInFlight(o))
                .GroupBy(o => o.Key)
                .Select(g => g.First())
                .Select(o => new RestoreItem
                {
                    ProductId = o.StoreProductId,
                    PurchaseToken = NullIfEmpty(o.PurchaseToken),
                    OrderId = string.IsNullOrEmpty(o.TransactionId) ? null : o.TransactionId,
                    StoreUserId = string.IsNullOrEmpty(o.StoreUserId) ? null : o.StoreUserId,
                })
                .ToList();
            // ... but what the player gets from them belongs to the answer: wait for them, as long as one request may take.
            await m_Orders.WaitForInFlightAsync(TimeSpan.FromSeconds(Math.Max(1, m_Config.requestTimeoutSeconds)), ct);
            ThrowIfDisposed();
            if (items.Count == 0)
                return await RefreshCustomerInfoAsync(ct);

            ApiResult<RestoreResponse> latest = null;
            var appInstanceId = await m_Context.AppInstanceIdAsync();
            var attribution = await m_Context.AttributionAsync();
            for (var i = 0; i < items.Count; i += RestoreRequest.MaxItems)
            {
                var request = new RestoreRequest
                {
                    AppId = AppId,
                    UserId = user,
                    AppInstanceId = appInstanceId,
                    Attribution = attribution,
                    Purchases = items.Skip(i).Take(RestoreRequest.MaxItems).ToList(),
                    Client = m_Client,
                };
                var response = await m_Api.RestoreAsync(request, ct);
                if (response.IsHttpSuccess && response.Value?.CustomerInfo != null)
                    latest = response;
                else
                    m_Log.Warning("Restore request failed: " + response);
            }

            if (latest == null)
                return await RefreshCustomerInfoAsync(ct);
            m_Refresher.Set(user, latest.Value.CustomerInfo, new SignedAnswer(latest.RawBody, latest.Signature, true));
            return m_Refresher.Current;
        }

        /// <inheritdoc/>
        /// <exception cref="ObjectDisposedException">The service was disposed.</exception>
        public Task<CustomerInfo> RefreshCustomerInfoAsync(CancellationToken ct = default) =>
            m_Context.IsDisposed
                ? Task.FromException<CustomerInfo>(new ObjectDisposedException(nameof(BillingService)))
                : m_Refresher.RefreshAsync(ct);

        /// <inheritdoc/>
        /// <remarks>Subscriptions count only while the customer info is fresh (<see cref="BillingConfig.entitlementsMaxAgeDays"/>,
        /// <see cref="CustomerInfo.IsFresh"/>); non-consumables always do.</remarks>
        public bool IsActive(string productId) => m_Refresher.IsActive(productId);

        /// <inheritdoc/>
        public bool HasAccess(string accessLevel) => m_Refresher.HasAccess(accessLevel);

        /// <inheritdoc/>
        public DateTime? GetExpiration(string productId) => m_Refresher.GetExpiration(productId);

        // ---- ICustomerInfoHost ----

        string ICustomerInfoHost.SyncUser() => SyncUser(refreshOnChange: false);

        void ICustomerInfoHost.OnCustomerInfoChanged(CustomerInfo info)
        {
            try
            {
                CustomerInfoChanged?.Invoke(info);
            }
            catch (Exception e)
            {
                m_Log.Error($"{nameof(CustomerInfoChanged)} handler threw: {e}");
            }
        }
    }
}
