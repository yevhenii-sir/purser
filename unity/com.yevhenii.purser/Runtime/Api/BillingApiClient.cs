using System;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>Answer of a billing server call: the HTTP status, the parsed body and an error text.</summary>
    public sealed class ApiResult<T> where T : class
    {
        /// <summary>HTTP status; 0 when the server was not reached.</summary>
        public int HttpStatus { get; }

        /// <summary>Parsed body (also present for 4xx/5xx answers with a JSON body).</summary>
        public T Value { get; }

        /// <summary>Transport error, unparsable body or the non-2xx status with the start of the body; null on success.</summary>
        public string Error { get; }

        /// <summary>The server was not reached (<see cref="HttpStatus"/> is 0): worth retrying.</summary>
        public bool IsTransportError => HttpStatus == 0;

        /// <summary>The server answered with a 2xx status.</summary>
        public bool IsHttpSuccess => HttpStatus >= 200 && HttpStatus < 300;

        /// <summary>Creates a result (custom <see cref="IBillingApi"/> implementations, tests).</summary>
        public ApiResult(int httpStatus, T value, string error)
        {
            HttpStatus = httpStatus;
            Value = value;
            Error = error;
        }

        /// <inheritdoc/>
        public override string ToString() => $"HTTP {HttpStatus} {Error}".Trim();
    }

    /// <summary>Billing server HTTP API (see server/README.md).</summary>
    public interface IBillingApi
    {
        /// <summary><c>POST /verify/{store}</c> (<see cref="BillingStores"/>).</summary>
        Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default);
        /// <summary><c>POST /restore</c>: verifies the purchases the store reports and returns the entitlements.</summary>
        Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default);
        /// <summary><c>POST /entitlements</c>: what the player owns.</summary>
        Task<ApiResult<EntitlementsDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default);
        /// <summary><c>GET /healthz</c>; the body is the raw JSON.</summary>
        Task<ApiResult<string>> HealthAsync(CancellationToken ct = default);
        /// <summary>What to show at a placement (paywall, products, remote config, A/B test variant).</summary>
        Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default);
        /// <summary>Reports a paywall view for A/B test results.</summary>
        Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default);
        /// <summary>Sets custom attributes of a player (<c>POST /attributes</c>).</summary>
        Task<ApiResult<AttributesResponse>> AttributesAsync(AttributesRequest request, CancellationToken ct = default);
    }

    /// <summary>Default <see cref="IBillingApi"/> over HTTP. Outside the Editor every request throws
    /// <see cref="BillingException"/> unless the URL is https (plain http only for localhost).</summary>
    public sealed partial class BillingApiClient : IBillingApi
    {
        private readonly JsonHttp m_Http;

        /// <param name="baseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        public BillingApiClient(string baseUrl, int timeoutSeconds = BillingConfig.DefaultRequestTimeoutSeconds,
            IHttpTransport transport = null)
        {
            m_Http = new JsonHttp(baseUrl, timeoutSeconds, transport);
        }

        /// <summary>Client for the config's server URL and request timeout.</summary>
        public BillingApiClient(BillingConfig config, IHttpTransport transport = null)
            : this(config.serverBaseUrl, config.requestTimeoutSeconds, transport) { }

        /// <inheritdoc/>
        public Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default)
        {
            if (string.IsNullOrEmpty(store))
                return Task.FromResult(new ApiResult<VerifyResponse>(0, null, "no billing store for the purchase"));
            return PostAsync<VerifyResponse>("/verify/" + store, request, ct);
        }

        /// <inheritdoc/>
        public Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default) =>
            PostAsync<RestoreResponse>("/restore", request, ct);

        /// <inheritdoc/>
        public Task<ApiResult<EntitlementsDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default) =>
            PostAsync<EntitlementsDto>("/entitlements", request, ct);

        /// <inheritdoc/>
        public Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default) =>
            PostAsync<PaywallResponse>("/paywall", request, ct);

        /// <inheritdoc/>
        public Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default) =>
            PostAsync<StatusResponse>("/paywall/event", request, ct);

        /// <inheritdoc/>
        public Task<ApiResult<AttributesResponse>> AttributesAsync(AttributesRequest request, CancellationToken ct = default) =>
            PostAsync<AttributesResponse>("/attributes", request, ct);

        /// <inheritdoc/>
        public async Task<ApiResult<string>> HealthAsync(CancellationToken ct = default)
        {
            var res = await m_Http.GetAsync("/healthz", ct);
            return new ApiResult<string>(res.Status, res.Body, res.Error);
        }

        private Task<ApiResult<T>> PostAsync<T>(string path, object body, CancellationToken ct) where T : class =>
            m_Http.PostAsync<T>(path, body, ct);
    }
}
