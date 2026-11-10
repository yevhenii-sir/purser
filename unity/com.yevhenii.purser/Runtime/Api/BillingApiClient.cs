using System;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

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
        Task<ApiResult<VerifyResponse>> VerifyAsync(BillingPlatform platform, VerifyRequest request, CancellationToken ct = default);
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
        private readonly string m_BaseUrl;
        private readonly int m_TimeoutSeconds;
        private readonly IHttpTransport m_Transport;

        public BillingApiClient(string baseUrl, int timeoutSeconds = 10, IHttpTransport transport = null)
        {
            if (string.IsNullOrWhiteSpace(baseUrl))
                throw new ArgumentException("Billing server URL is empty", nameof(baseUrl));
            m_BaseUrl = baseUrl.TrimEnd('/');
            m_TimeoutSeconds = Math.Max(1, timeoutSeconds);
            m_Transport = transport ?? new UnityWebRequestTransport();
        }

        public BillingApiClient(BillingConfig config, IHttpTransport transport = null)
            : this(config.serverBaseUrl, config.requestTimeoutSeconds, transport) { }

        /// <summary><c>POST /verify/{store}</c>: <see cref="VerifyRequest.Store"/> when set, else the platform's store.</summary>
        public Task<ApiResult<VerifyResponse>> VerifyAsync(BillingPlatform platform, VerifyRequest request, CancellationToken ct = default)
        {
            var store = string.IsNullOrEmpty(request?.Store) ? platform.StoreName() : request.Store;
            if (string.IsNullOrEmpty(store))
                return Task.FromResult(new ApiResult<VerifyResponse>(0, null, $"no billing store for platform {platform}"));
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
            var res = await m_Transport.SendAsync("GET", m_BaseUrl + "/healthz", null, m_TimeoutSeconds, ct);
            return new ApiResult<string>(res.Status, res.Body, res.Error);
        }

        private async Task<ApiResult<T>> PostAsync<T>(string path, object body, CancellationToken ct) where T : class
        {
            var json = JsonConvert.SerializeObject(body);
            var res = await m_Transport.SendAsync("POST", m_BaseUrl + path, json, m_TimeoutSeconds, ct);
            if (res.IsTransportError)
                return new ApiResult<T>(0, null, res.Error);

            T value = null;
            string error = null;
            if (!string.IsNullOrEmpty(res.Body))
            {
                try
                {
                    value = JsonConvert.DeserializeObject<T>(res.Body);
                }
                catch (JsonException e)
                {
                    error = $"invalid JSON from server: {e.Message}";
                }
            }
            if (error == null && (res.Status < 200 || res.Status >= 300))
                error = $"HTTP {res.Status}: {Truncate(res.Body)}";
            return new ApiResult<T>(res.Status, value, error);
        }

        private static string Truncate(string s) => s == null ? "" : s.Length <= 300 ? s : s.Substring(0, 300);
    }
}
