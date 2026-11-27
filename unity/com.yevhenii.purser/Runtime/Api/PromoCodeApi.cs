using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>Body of <c>POST /redeem</c>.</summary>
    public sealed class RedeemRequest
    {
        [JsonProperty("app_id")] public string AppId;
        /// <summary>Store of the build the request is for (<see cref="BillingStores"/>): the server finds the build by
        /// this store and <c>app_id</c>, the app's id in that store.</summary>
        [JsonProperty("store")] public string Store;
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("code")] public string Code;
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    /// <summary>
    /// Promo codes on the billing server (<c>POST /redeem</c>: the answer has the shape of <c>/verify</c>). Implemented
    /// by <see cref="BillingApiClient"/>; a custom <see cref="IBillingApi"/> implements it too to support
    /// <see cref="IBillingService.RedeemCodeAsync"/>.
    /// </summary>
    public interface IPromoCodeApi
    {
        Task<ApiResult<VerifyResponse>> RedeemAsync(RedeemRequest request, CancellationToken ct = default);
    }

    public sealed partial class BillingApiClient : IPromoCodeApi
    {
        public Task<ApiResult<VerifyResponse>> RedeemAsync(RedeemRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<VerifyResponse>("/redeem", request, request?.UserId, ct);
    }
}
