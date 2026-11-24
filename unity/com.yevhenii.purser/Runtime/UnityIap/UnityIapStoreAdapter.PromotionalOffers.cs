namespace Purser
{
    public sealed partial class UnityIapStoreAdapter : IApplePromotionalOfferStore
    {
        /// <summary>
        /// App Store purchase with a signed promotional offer through Unity IAP's StoreKit 2 bridge
        /// (see <see cref="UnityIapApplePromotionalOffers"/>). Reports <see cref="PurchaseFailed"/> instead of
        /// buying at the full price when the offer cannot be applied.
        /// </summary>
        public void PurchaseWithPromotionalOffer(string storeProductId, ApplePromotionalOffer offer)
        {
            if (Platform != BillingPlatform.Apple)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId,
                    "promotional offer signatures are App Store only", false));
                return;
            }
            var product = m_Store.GetProductById(storeProductId);
            if (product == null || !product.availableToPurchase || product.definition == null)
            {
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, "product is not available in the store", false));
                return;
            }
            if (!UnityIapApplePromotionalOffers.TryPurchase(product.definition, offer, out var error))
            {
                m_Log.Error($"Promotional offer {offer} not started: {error}");
                PurchaseFailed?.Invoke(new StorePurchaseFailure(storeProductId, error, false));
            }
        }
    }
}
