using System;

namespace Purser
{
    /// <summary>
    /// An App Store promotional offer signed by the billing server (<c>POST /offers/apple/sign</c>).
    /// Valid for one purchase attempt within 24 hours, and only for a purchase made with
    /// <see cref="AppAccountToken"/>. <see cref="IBillingService.PurchaseWithPromotionalOfferAsync"/> uses it
    /// for you; get one with <see cref="IBillingService.GetApplePromotionalOfferAsync"/> only when you buy
    /// through your own StoreKit code.
    /// </summary>
    public sealed class ApplePromotionalOffer
    {
        /// <summary>App Store product id of the subscription.</summary>
        public string ProductId { get; }

        /// <summary>Promotional offer identifier from App Store Connect.</summary>
        public string OfferId { get; }

        /// <summary>Id of the In-App Purchase key that signed the offer (StoreKit <c>keyID</c>).</summary>
        public string KeyId { get; }

        /// <summary>One-time lowercase UUID.</summary>
        public string Nonce { get; }

        /// <summary>Unix time in milliseconds the signature was made at.</summary>
        public long Timestamp { get; }

        /// <summary>Base64 DER ECDSA signature (StoreKit <c>signature</c>).</summary>
        public string Signature { get; }

        /// <summary><c>appAccountToken</c> the purchase must be made with (derived from the user id, see <see cref="AccountIds"/>).</summary>
        public string AppAccountToken { get; }

        /// <summary>
        /// Promotional offer V2 signature (compact JWS) for StoreKit 2
        /// <c>Product.PurchaseOption.promotionalOffer(_:compactJWS:)</c>; null when the server has no issuer id.
        /// Unity IAP does not use it.
        /// </summary>
        public string SignedJws { get; }

        /// <summary>When Apple stops accepting the signature (UTC).</summary>
        public DateTime ExpiresAt => DateTimeOffset.FromUnixTimeMilliseconds(Timestamp).UtcDateTime.AddHours(24);

        public ApplePromotionalOffer(string productId, string offerId, string keyId, string nonce, long timestamp,
            string signature, string appAccountToken, string signedJws = null)
        {
            ProductId = productId;
            OfferId = offerId;
            KeyId = keyId;
            Nonce = nonce;
            Timestamp = timestamp;
            Signature = signature;
            AppAccountToken = appAccountToken;
            SignedJws = signedJws;
        }

        /// <summary>
        /// Checks the fields StoreKit parses. Unity IAP silently drops an offer it cannot parse and the
        /// purchase would go through at the full price, so an invalid offer must never reach the store.
        /// </summary>
        public bool IsWellFormed(out string problem)
        {
            problem = null;
            if (string.IsNullOrEmpty(ProductId) || string.IsNullOrEmpty(OfferId) || string.IsNullOrEmpty(KeyId))
                problem = "product id, offer id and key id are required";
            else if (!Guid.TryParse(Nonce, out _))
                problem = "nonce is not a UUID";
            else if (!Guid.TryParse(AppAccountToken, out _))
                problem = "appAccountToken is not a UUID";
            else if (Timestamp <= 0)
                problem = "timestamp is missing";
            else if (!IsBase64(Signature))
                problem = "signature is not base64";
            return problem == null;
        }

        private static bool IsBase64(string s)
        {
            if (string.IsNullOrEmpty(s))
                return false;
            try
            {
                return Convert.FromBase64String(s).Length > 0;
            }
            catch (FormatException)
            {
                return false;
            }
        }

        public override string ToString() => $"{ProductId}/{OfferId}";
    }
}
