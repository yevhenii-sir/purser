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

        /// <summary>Parsed body (also present for 4xx/5xx answers with a JSON body). Null when the answer failed its
        /// signature check (<see cref="BillingConfig.responseSigningPublicKey"/>).</summary>
        public T Value { get; }

        /// <summary>Transport error, unparsable body, a failed signature check, or the non-2xx status with the start of
        /// the body; null on success.</summary>
        public string Error { get; }

        /// <summary>The <c>code</c> of an error answer (<see cref="ErrorCodes"/>), or null.</summary>
        public string Code { get; }

        /// <summary>A 429 answer's <c>Retry-After</c> (at most 60 seconds); null otherwise.</summary>
        public TimeSpan? RetryAfter { get; }

        /// <summary>The server was not reached (<see cref="HttpStatus"/> is 0): worth retrying.</summary>
        public bool IsTransportError => HttpStatus == 0;

        /// <summary>The server answered with a 2xx status.</summary>
        public bool IsHttpSuccess => HttpStatus >= 200 && HttpStatus < 300;

        /// <summary>Too many requests (429): retryable after <see cref="RetryAfter"/>.</summary>
        public bool IsRateLimited => HttpStatus == 429;

        /// <summary>Creates a result (custom <see cref="IBillingApi"/> implementations, tests).</summary>
        public ApiResult(int httpStatus, T value, string error, string code = null, TimeSpan? retryAfter = null)
        {
            HttpStatus = httpStatus;
            Value = value;
            Error = error;
            Code = code;
            RetryAfter = retryAfter;
        }

        /// <inheritdoc/>
        public override string ToString() => $"HTTP {HttpStatus} {Error}".Trim();
    }

    /// <summary>Billing server HTTP API (see server/README.md).</summary>
    public interface IBillingApi
    {
        /// <summary><c>POST /verify/{store}</c> (<see cref="BillingStores"/>).</summary>
        Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default);
        /// <summary><c>POST /restore</c>: verifies the purchases the store reports and returns the customer info.</summary>
        Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default);
        /// <summary><c>POST /entitlements</c>: what gives the player access now.</summary>
        Task<ApiResult<CustomerInfoDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default);
        /// <summary><c>GET /healthz</c>; the body is the raw JSON.</summary>
        Task<ApiResult<string>> HealthAsync(CancellationToken ct = default);
        /// <summary>What to show at a placement (paywall, products, remote config, A/B test variant).</summary>
        Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default);
        /// <summary>Reports a paywall view for A/B test results.</summary>
        Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default);
        /// <summary>Sets custom attributes of a player (<c>POST /attributes</c>).</summary>
        Task<ApiResult<AttributesResponse>> AttributesAsync(AttributesRequest request, CancellationToken ct = default);
    }

    /// <summary>
    /// Default <see cref="IBillingApi"/> over HTTP. Outside the Editor every request throws
    /// <see cref="BillingException"/> unless the URL is https (plain http only for localhost). With a response signing
    /// key, <c>/entitlements</c> and <c>/restore</c> answers without a valid <c>X-Purser-Signature</c> are errors.
    /// Signs requests with player tokens once given an <see cref="IPlayerTokenProvider"/>.
    /// </summary>
    public sealed partial class BillingApiClient : IBillingApi, IPlayerTokenConsumer
    {
        private readonly JsonHttp m_Http;
        private readonly string m_SigningKey;

        /// <param name="baseUrl">Billing server, <c>https://&lt;host&gt;/v1/database/&lt;db&gt;/route</c>.</param>
        /// <param name="responseSigningPublicKey">Base64 Ed25519 public key of signed responses
        /// (<see cref="BillingConfig.responseSigningPublicKey"/>); empty = answers are not checked. A key that is not
        /// 32 bytes of base64 fails every checked answer.</param>
        public BillingApiClient(string baseUrl, int timeoutSeconds = BillingConfig.DefaultRequestTimeoutSeconds,
            IHttpTransport transport = null, string responseSigningPublicKey = null)
        {
            m_Http = new JsonHttp(baseUrl, timeoutSeconds, transport);
            m_SigningKey = string.IsNullOrWhiteSpace(responseSigningPublicKey) ? null : responseSigningPublicKey.Trim();
        }

        /// <summary>Client for the config's server URL, request timeout and response signing key.</summary>
        public BillingApiClient(BillingConfig config, IHttpTransport transport = null)
            : this(config.serverBaseUrl, config.requestTimeoutSeconds, transport, config.responseSigningPublicKey) { }

        /// <inheritdoc/>
        public void SetPlayerTokenProvider(IPlayerTokenProvider provider) => m_Http.Tokens = provider;

        /// <inheritdoc/>
        public Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default)
        {
            if (string.IsNullOrEmpty(store))
                return Task.FromResult(new ApiResult<VerifyResponse>(0, null, "no billing store for the purchase"));
            return m_Http.PostAsync<VerifyResponse>("/verify/" + store, request, request?.UserId, ct);
        }

        /// <inheritdoc/>
        public Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<RestoreResponse>("/restore", request, request?.UserId, ct, CheckSignature);

        /// <inheritdoc/>
        public Task<ApiResult<CustomerInfoDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<CustomerInfoDto>("/entitlements", request, request?.UserId, ct, CheckSignature);

        /// <inheritdoc/>
        public Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<PaywallResponse>("/paywall", request, request?.UserId, ct);

        /// <inheritdoc/>
        public Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<StatusResponse>("/paywall/event", request, request?.UserId, ct);

        /// <inheritdoc/>
        public Task<ApiResult<AttributesResponse>> AttributesAsync(AttributesRequest request, CancellationToken ct = default) =>
            m_Http.PostAsync<AttributesResponse>("/attributes", request, request?.UserId, ct);

        /// <inheritdoc/>
        public async Task<ApiResult<string>> HealthAsync(CancellationToken ct = default)
        {
            var res = await m_Http.GetAsync("/healthz", ct);
            return new ApiResult<string>(res.Status, res.Body, res.Error);
        }

        /// <summary>Null when signed responses are off or the answer's signature is valid.</summary>
        private string CheckSignature(HttpResponse res) =>
            m_SigningKey == null ? null : ResponseSignature.Problem(m_SigningKey, res.RawBody, res.GetHeader(ResponseSignature.Header));
    }
}
