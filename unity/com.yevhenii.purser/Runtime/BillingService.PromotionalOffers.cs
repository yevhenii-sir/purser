using System;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    public sealed partial class BillingService
    {
        public async Task<ApplePromotionalOffer> GetApplePromotionalOfferAsync(string productId, string offerId,
            CancellationToken ct = default)
        {
            ThrowIfDisposed();
            RequireAppStore();
            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId)
                ?? throw new BillingException($"Unknown product {productId}");
            var user = CurrentUser() ?? throw new BillingException("User id is not set");
            var (offer, error) = await SignOfferAsync(config.StoreId(m_Store.Store), offerId, user, ct);
            return offer ?? throw new BillingException(error);
        }

        public async Task<PurchaseResult> PurchaseWithPromotionalOfferAsync(string productId, string offerId,
            CancellationToken ct = default)
        {
            if (!IsInitialized || m_Disposed)
                return new PurchaseResult(PurchaseStatus.NotInitialized, productId, message: "billing is not initialized");
            if (m_IsEditor && Platform != BillingPlatform.Apple)
            {
                m_Log.Warning($"Promotional offer {offerId} needs an App Store device; buying {productId} without it in the Editor.");
                return await PurchaseAsync(productId, ct);
            }
            RequireAppStore();

            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId);
            if (config == null)
                return new PurchaseResult(PurchaseStatus.Failed, productId, message: "unknown product");
            var storeId = config.StoreId(m_Store.Store);
            if (config.type != BillingProductType.Subscription)
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId,
                    message: "promotional offers apply to subscriptions only");
            if (string.IsNullOrEmpty(offerId))
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: "offer id is empty");
            var user = CurrentUser();
            if (user == null)
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: "user id is not set");
            if (!(m_Store is IApplePromotionalOfferStore offerStore))
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId,
                    message: "the store adapter does not support promotional offers");
            if (m_Waiters.ContainsKey(storeId))
                return new PurchaseResult(PurchaseStatus.AlreadyInProgress, config.id, storeId);

            // Hold the product while the signature is fetched, so a second tap does not start another purchase.
            var waiter = new TaskCompletionSource<PurchaseResult>();
            m_Waiters[storeId] = waiter;
            try
            {
                var (offer, error) = await SignOfferAsync(storeId, offerId, user, ct);
                if (offer == null || m_Disposed || CurrentUser() != user)
                {
                    m_Waiters.Remove(storeId);
                    return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId,
                        message: error ?? "the player changed while the offer was being signed");
                }
                BindAccount();
                offerStore.PurchaseWithPromotionalOffer(storeId, offer);
            }
            catch (Exception e)
            {
                m_Waiters.Remove(storeId);
                if (e is OperationCanceledException)
                    throw;
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: e.Message);
            }

            using (ct.Register(() => waiter.TrySetCanceled()))
            {
                try
                {
                    return await waiter.Task;
                }
                finally
                {
                    if (m_Waiters.TryGetValue(storeId, out var current) && current == waiter)
                        m_Waiters.Remove(storeId);
                }
            }
        }

        private void RequireAppStore()
        {
            if (Platform != BillingPlatform.Apple)
                throw new BillingException(
                    "Promotional offer signatures are App Store only. On Google Play, subscription offers are " +
                    "chosen on the client by their offer token (base plan offers in Play Console) and need no server signature.");
        }

        /// <summary>Asks the server for a signature; returns (offer, null) or (null, error).</summary>
        private async Task<(ApplePromotionalOffer, string)> SignOfferAsync(string storeId, string offerId, string user,
            CancellationToken ct)
        {
            if (!(m_Api is IApplePromotionalOfferApi api))
                return (null, "the billing API client does not support promotional offers");
            var response = await api.SignApplePromotionalOfferAsync(new ApplePromotionalOfferRequest
            {
                BundleId = AppId,
                ProductId = storeId,
                OfferId = offerId,
                UserId = user,
            }, ct);
            var body = response?.Value;
            if (response == null || !response.IsHttpSuccess || body?.Status != VerifyStatus.Success)
            {
                var reason = body?.Message ?? response?.Error ?? "no response";
                m_Log.Warning($"Promotional offer {storeId}/{offerId} was not signed: {reason}");
                return (null, "promotional offer was not signed: " + reason);
            }

            var offer = body.ToOffer();
            if (!offer.IsWellFormed(out var problem))
                return (null, "server returned an invalid promotional offer: " + problem);
            // The signature covers the appAccountToken: the purchase must carry the same one.
            if (!Guid.TryParse(offer.AppAccountToken, out var token) || token != AccountIds.AppleAccountToken(user))
                return (null, "server signed the offer for another appAccountToken");
            if (offer.ProductId != storeId || offer.OfferId != offerId)
                return (null, "server signed another product or offer");
            return (offer, null);
        }
    }
}
