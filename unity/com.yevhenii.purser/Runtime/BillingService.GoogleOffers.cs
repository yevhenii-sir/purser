using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Google Play subscription offers chosen by the game: listed by the store adapter (IGoogleOfferStore) and bought
    // through PurchaseAsync with PurchaseOptions.GoogleOfferToken.
    public sealed partial class BillingService
    {
        /// <summary>The <see cref="StoreOffer.RevenueId"/> of every offer token <see cref="GetGoogleOffersAsync"/>
        /// returned: a purchase with the token tells the server its offer.</summary>
        private readonly Dictionary<string, string> m_GoogleOfferIds = new Dictionary<string, string>();

        /// <inheritdoc/>
        public bool GoogleOffersSupported => !m_Context.IsDisposed && m_Store is IGoogleOfferStore store && store.GoogleOffersSupported;

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreOffer>> GetGoogleOffersAsync(string productId, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId)
                ?? throw new BillingException($"Unknown product {productId}");
            var store = GoogleOfferStore(config, out var problem) ?? throw new BillingException(problem);
            var offers = await store.GetGoogleOffersAsync(config.StoreId(m_Store.Store), ct);
            foreach (var offer in offers)
                if (offer.Token != null && offer.RevenueId != null)
                    m_GoogleOfferIds[offer.Token] = offer.RevenueId;
            return offers;
        }

        /// <summary>The store that buys Google offers of <paramref name="config"/>; null and the reason when there is
        /// none.</summary>
        private IGoogleOfferStore GoogleOfferStore(BillingProductConfig config, out string problem)
        {
            problem = null;
            if (config.type != BillingProductType.Subscription)
                problem = "Google offers apply to subscriptions only";
            else if (!(m_Store is IGoogleOfferStore store))
                problem = $"the {m_Store.Store} store adapter does not support Google Play offers";
            else if (!store.GoogleOffersSupported)
                problem = GooglePlayOfferClient.NotSupportedMessage;
            else
                return store;
            return null;
        }
    }
}
