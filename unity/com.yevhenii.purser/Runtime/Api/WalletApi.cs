using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>Body of <c>POST /wallet</c>.</summary>
    public sealed class WalletRequest
    {
        [JsonProperty("app_id")] public string AppId;
        /// <summary>Store of the build the request is for (<see cref="BillingStores"/>): the server finds the build by
        /// this store and <c>app_id</c>, the app's id in that store.</summary>
        [JsonProperty("store")] public string Store;
        [JsonProperty("user_id")] public string UserId;
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    /// <summary>Answer of <c>POST /wallet</c>.</summary>
    public sealed class WalletResponse
    {
        [JsonProperty("status")] public string Status;
        /// <summary>Balance per currency id; currencies the player never had are missing (0).</summary>
        [JsonProperty("balances")] public Dictionary<string, long> Balances;
    }

    /// <summary>Body of <c>POST /wallet/spend</c>.</summary>
    public sealed class SpendRequest
    {
        [JsonProperty("app_id")] public string AppId;
        /// <summary>Store of the build the request is for (<see cref="BillingStores"/>): the server finds the build by
        /// this store and <c>app_id</c>, the app's id in that store.</summary>
        [JsonProperty("store")] public string Store;
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("currency")] public string Currency;
        [JsonProperty("amount")] public long Amount;
        /// <summary>The game's id of this spend: a repeated request with it spends nothing more.</summary>
        [JsonProperty("reference")] public string Reference;
        [JsonProperty("note", NullValueHandling = NullValueHandling.Ignore)] public string Note;
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    /// <summary>Answer of <c>POST /wallet/spend</c>.</summary>
    public sealed class SpendResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("balance")] public long Balance;
        /// <summary>The reference was spent before: nothing changed now.</summary>
        [JsonProperty("repeated")] public bool Repeated;
        [JsonProperty("code")] public string Code;
        [JsonProperty("message")] public string Message;
    }

    /// <summary>
    /// The player's virtual currency on the billing server (<c>/wallet</c>, <c>/wallet/spend</c>). Implemented by
    /// <see cref="BillingApiClient"/>; a custom <see cref="IBillingApi"/> implements it too to support
    /// <see cref="IBillingService.GetWalletAsync"/> and <see cref="IBillingService.SpendAsync"/>.
    /// </summary>
    public interface IWalletApi
    {
        Task<ApiResult<WalletResponse>> GetWalletAsync(WalletRequest request, CancellationToken ct = default);
        Task<ApiResult<SpendResponse>> SpendAsync(SpendRequest request, CancellationToken ct = default);
    }

    public sealed partial class BillingApiClient : IWalletApi
    {
        public Task<ApiResult<WalletResponse>> GetWalletAsync(WalletRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<WalletResponse>("/wallet", request, request?.UserId, ct);

        public Task<ApiResult<SpendResponse>> SpendAsync(SpendRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<SpendResponse>("/wallet/spend", request, request?.UserId, ct);
    }
}
