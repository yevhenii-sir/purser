using System;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    public sealed partial class BillingService
    {
        /// <inheritdoc/>
        public async Task<bool> OpenManageSubscriptionsAsync(string productId = null, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            string url;
            if (m_Store is ISubscriptionManagementStore store)
            {
                url = await store.GetManageSubscriptionsUrlAsync(SyncUser(), ct);
            }
            else
            {
                var product = string.IsNullOrEmpty(productId) ? null : GetProduct(productId);
                var storeProductId = product != null && product.Type == BillingProductType.Subscription ? product.StoreId : null;
                url = ManageSubscriptionsUrl(m_Store.Store, AppId, storeProductId);
            }
            if (url == null)
            {
                m_Log.Info($"No subscription management page for the {m_Store.Store} store: players manage subscriptions in the store's own app or account");
                return false;
            }
            m_OpenUrl(url);
            return true;
        }

        /// <inheritdoc/>
        public bool PresentOfferCodeRedemption()
        {
            ThrowIfDisposed();
            if (!IsInitialized || !(m_Store is IOfferCodeRedemptionStore codes))
                return false;
            return codes.PresentOfferCodeRedemption();
        }

        /// <summary>The store's page where players manage their subscriptions; null for stores without one.</summary>
        /// <param name="store">Store id (<see cref="BillingStores"/>).</param>
        /// <param name="appId">The build's app id (Google: the package name) for a link to one subscription.</param>
        /// <param name="storeProductId">The store product id of the subscription to open, or null for all of them.</param>
        internal static string ManageSubscriptionsUrl(string store, string appId, string storeProductId)
        {
            switch (store)
            {
                case BillingStores.Google:
                    // https://developer.android.com/google/play/billing/subscriptions#deep-link
                    const string google = "https://play.google.com/store/account/subscriptions";
                    if (string.IsNullOrEmpty(storeProductId) || string.IsNullOrEmpty(appId))
                        return google;
                    return $"{google}?sku={Uri.EscapeDataString(storeProductId)}&package={Uri.EscapeDataString(appId)}";
                case BillingStores.Apple:
                    // Opens the App Store's subscriptions page (the same list as Settings > Apple ID > Subscriptions).
                    return "https://apps.apple.com/account/subscriptions";
                default:
                    return null;
            }
        }
    }
}
