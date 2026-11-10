using System;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    public sealed class ApiResult<T> where T : class
    {
        /// <summary>HTTP status; 0 when the server was not reached.</summary>
        public int HttpStatus { get; }

        /// <summary>Parsed body (also present for 4xx/5xx answers with a JSON body).</summary>
        public T Value { get; }

        public string Error { get; }

        public bool IsTransportError => HttpStatus == 0;

        public bool IsHttpSuccess => HttpStatus >= 200 && HttpStatus < 300;

        public ApiResult(int httpStatus, T value, string error)
        {
            HttpStatus = httpStatus;
            Value = value;
            Error = error;
        }

        public override string ToString() => $"HTTP {HttpStatus} {Error}".Trim();
    }

    /// <summary>Billing server HTTP API (see server/README.md).</summary>
    public interface IBillingApi
    {
        /// <summary><c>POST /verify/{store}</c> (<see cref="BillingStores"/>).</summary>
        Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default);
        Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default);
        Task<ApiResult<EntitlementsDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default);
        Task<ApiResult<string>> HealthAsync(CancellationToken ct = default);
        /// <summary>What to show at a placement (paywall, products, remote config, A/B test variant).</summary>
        Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default);
        /// <summary>Reports a paywall view for A/B test results.</summary>
        Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default);
    }

    public sealed partial class BillingApiClient : IBillingApi
    {
        private readonly JsonHttp m_Http;

        public BillingApiClient(string baseUrl, int timeoutSeconds = 10, IHttpTransport transport = null)
        {
            m_Http = new JsonHttp(baseUrl, timeoutSeconds, transport);
        }

        public BillingApiClient(BillingConfig config, IHttpTransport transport = null)
            : this(config.serverBaseUrl, config.requestTimeoutSeconds, transport) { }

        public Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default)
        {
            if (string.IsNullOrEmpty(store))
                return Task.FromResult(new ApiResult<VerifyResponse>(0, null, "no billing store for the purchase"));
            return PostAsync<VerifyResponse>("/verify/" + store, request, ct);
        }

        public Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default) =>
            PostAsync<RestoreResponse>("/restore", request, ct);

        public Task<ApiResult<EntitlementsDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default) =>
            PostAsync<EntitlementsDto>("/entitlements", request, ct);

        public Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default) =>
            PostAsync<PaywallResponse>("/paywall", request, ct);

        public Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default) =>
            PostAsync<StatusResponse>("/paywall/event", request, ct);

        public async Task<ApiResult<string>> HealthAsync(CancellationToken ct = default)
        {
            var res = await m_Http.GetAsync("/healthz", ct);
            return new ApiResult<string>(res.Status, res.Body, res.Error);
        }

        private Task<ApiResult<T>> PostAsync<T>(string path, object body, CancellationToken ct) where T : class =>
            m_Http.PostAsync<T>(path, body, ct);
    }
}
