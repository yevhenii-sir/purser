namespace Purser
{
    /// <summary>
    /// Optional <see cref="IStoreAdapter"/> capability: an App Store purchase with a signed promotional offer.
    /// The result arrives like any purchase: <see cref="IStoreAdapter.OrderPending"/> or
    /// <see cref="IStoreAdapter.PurchaseFailed"/>. Implemented by <see cref="UnityIapStoreAdapter"/>.
    /// </summary>
    public interface IApplePromotionalOfferStore
    {
        /// <summary>
        /// Starts the purchase with the offer and its <see cref="ApplePromotionalOffer.AppAccountToken"/>.
        /// Must report a failure (never buy at the full price) when the offer cannot be applied.
        /// </summary>
        void PurchaseWithPromotionalOffer(string storeProductId, ApplePromotionalOffer offer);
    }
}
