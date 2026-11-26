using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// Optional <see cref="IStoreAdapter"/> capability: Google Play subscription offers chosen by the game. Lists the
    /// offers of a subscription (<see cref="IBillingService.GetGoogleOffersAsync"/>) and buys one by its offer token
    /// (<see cref="PurchaseOptions.GoogleOfferToken"/>). The result arrives like any purchase:
    /// <see cref="IStoreAdapter.OrderPending"/>, <see cref="IStoreAdapter.OrderDeferred"/>,
    /// <see cref="IStoreAdapter.PurchaseFailed"/>, or <see cref="ISubscriptionChangeStore.ChangeDeferred"/> for a deferred
    /// replacement. Implemented by <c>UnityIapStoreAdapter</c> on Google Play (with <see cref="GooglePlayOfferClient"/>).
    /// </summary>
    public interface IGoogleOfferStore
    {
        /// <summary>Offers can be listed and bought now (a Google Play build on an Android device with the package's
        /// Android plugin); false in the Editor and on other stores.</summary>
        bool GoogleOffersSupported { get; }

        /// <summary>The base plans and offers of a subscription the player can buy, with their tokens. Throws
        /// <see cref="BillingException"/> when the store cannot list them.</summary>
        Task<IReadOnlyList<StoreOffer>> GetGoogleOffersAsync(string storeProductId, CancellationToken ct);

        /// <summary>
        /// Buys <paramref name="storeProductId"/> with the offer of <paramref name="offerToken"/>; with
        /// <paramref name="replacedStoreProductId"/> (null: none) in place of that subscription of the player, as
        /// <paramref name="mode"/> says. Must report a failure (never buy another offer) when the offer cannot be bought.
        /// </summary>
        void PurchaseWithGoogleOffer(string storeProductId, string offerToken, string replacedStoreProductId, ReplacementMode mode);
    }
}
