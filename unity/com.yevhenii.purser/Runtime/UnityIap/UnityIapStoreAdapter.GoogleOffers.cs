using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    public sealed partial class UnityIapStoreAdapter : IGoogleOfferStore
    {
        /// <summary>Google Play offers next to Unity IAP (which buys only the first offer of a product); null on the
        /// App Store.</summary>
        private GooglePlayOfferClient m_GoogleOffers;

        /// <summary>Google <c>obfuscatedAccountId</c> of the bound player, for offer purchases.</summary>
        private string m_GoogleAccountId;

        /// <inheritdoc/>
        public bool GoogleOffersSupported => m_GoogleOffers != null && m_GoogleOffers.IsSupported;

        private void CreateGoogleOffers()
        {
            if (Platform != BillingPlatform.Google)
                return;
            m_GoogleOffers = new GooglePlayOfferClient(new AndroidGooglePlayBillingBridge(), m_Log);
            m_GoogleOffers.OrderPending += RaiseOrderPending;
            m_GoogleOffers.OrderDeferred += RaiseOrderDeferred;
            m_GoogleOffers.PurchaseFailed += RaisePurchaseFailed;
            m_GoogleOffers.ChangeDeferred += RaiseChangeDeferred;
        }

        private void DisposeGoogleOffers()
        {
            if (m_GoogleOffers == null)
                return;
            m_GoogleOffers.OrderPending -= RaiseOrderPending;
            m_GoogleOffers.OrderDeferred -= RaiseOrderDeferred;
            m_GoogleOffers.PurchaseFailed -= RaisePurchaseFailed;
            m_GoogleOffers.ChangeDeferred -= RaiseChangeDeferred;
        }

        /// <inheritdoc/>
        public Task<IReadOnlyList<StoreOffer>> GetGoogleOffersAsync(string storeProductId, CancellationToken ct) =>
            m_GoogleOffers != null
                ? m_GoogleOffers.GetOffersAsync(storeProductId, ct)
                : Task.FromException<IReadOnlyList<StoreOffer>>(new BillingException(GooglePlayOfferClient.NotSupportedMessage));

        /// <summary>
        /// Google Play: buys the offer through the package's Android plugin, with a Billing Library client of its own
        /// (Unity IAP's is not touched). The purchase arrives through <see cref="OrderPending"/> with the purchase
        /// token, is verified by the server and acknowledged there: <see cref="Finish"/> does nothing for it. Unity IAP
        /// lists it among the player's purchases afterwards.
        /// </summary>
        public void PurchaseWithGoogleOffer(string storeProductId, string offerToken, string replacedStoreProductId,
            ReplacementMode mode)
        {
            if (m_GoogleOffers == null)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, GooglePlayOfferClient.NotSupportedMessage, false));
                return;
            }
            m_GoogleOffers.Purchase(new GooglePlayBillingFlow
            {
                ProductId = storeProductId,
                OfferToken = offerToken,
                OldProductId = replacedStoreProductId,
                ReplacementMode = mode,
                ObfuscatedAccountId = m_GoogleAccountId,
            });
        }

        private void RaiseOrderPending(StoreOrder order) => OrderPending?.Invoke(order);
        private void RaiseOrderDeferred(StoreOrder order) => OrderDeferred?.Invoke(order);
        private void RaisePurchaseFailed(StorePurchaseFailure failure) => PurchaseFailed?.Invoke(failure);
        private void RaiseChangeDeferred(string storeProductId) => ChangeDeferred?.Invoke(storeProductId);
    }
}
