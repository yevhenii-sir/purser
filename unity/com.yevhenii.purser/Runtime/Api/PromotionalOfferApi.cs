using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>Body of <c>POST /offers/apple/sign</c>.</summary>
    public sealed class ApplePromotionalOfferRequest
    {
        [JsonProperty("bundle_id")] public string BundleId;
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("offer_id")] public string OfferId;
        [JsonProperty("user_id")] public string UserId;
        /// <summary>Optional App Store transaction id of the player, put into the V2 JWS.</summary>
        [JsonProperty("transaction_id", NullValueHandling = NullValueHandling.Ignore)] public string TransactionId;
    }

    public sealed class ApplePromotionalOfferResponse
    {
        /// <summary><c>success</c> or <c>error</c>.</summary>
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("offer_id")] public string OfferId;
        [JsonProperty("key_id")] public string KeyId;
        [JsonProperty("nonce")] public string Nonce;
        [JsonProperty("timestamp")] public long Timestamp;
        [JsonProperty("signature")] public string Signature;
        [JsonProperty("app_account_token")] public string AppAccountToken;
        [JsonProperty("signed_offer_jws")] public string SignedOfferJws;
        [JsonProperty("expires_at_ms")] public long ExpiresAtMs;

        public ApplePromotionalOffer ToOffer() =>
            new ApplePromotionalOffer(ProductId, OfferId, KeyId, Nonce, Timestamp, Signature, AppAccountToken, SignedOfferJws);
    }

    /// <summary>
    /// Promotional offer signatures from the billing server. Implemented by <see cref="BillingApiClient"/>;
    /// a custom <see cref="IBillingApi"/> implements it too to support
    /// <see cref="IBillingService.PurchaseWithPromotionalOfferAsync"/>.
    /// </summary>
    public interface IApplePromotionalOfferApi
    {
        Task<ApiResult<ApplePromotionalOfferResponse>> SignApplePromotionalOfferAsync(ApplePromotionalOfferRequest request,
            CancellationToken ct = default);
    }

    public sealed partial class BillingApiClient : IApplePromotionalOfferApi
    {
        public Task<ApiResult<ApplePromotionalOfferResponse>> SignApplePromotionalOfferAsync(ApplePromotionalOfferRequest request,
            CancellationToken ct = default) =>
            PostAsync<ApplePromotionalOfferResponse>("/offers/apple/sign", request, ct);
    }
}
