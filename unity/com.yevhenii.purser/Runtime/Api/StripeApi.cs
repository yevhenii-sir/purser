using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>Body of <c>POST /checkout/stripe</c>.</summary>
    public sealed class StripeCheckoutRequest
    {
        [JsonProperty("app_id")] public string AppId;
        /// <summary>Product id of the Stripe build on the server = the Stripe Price id (<c>price_...</c>).</summary>
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("user_id")] public string UserId;
        /// <summary>https page Stripe returns to after payment; may contain <c>{CHECKOUT_SESSION_ID}</c>.</summary>
        [JsonProperty("success_url")] public string SuccessUrl;
        /// <summary>https page Stripe returns to when the player goes back.</summary>
        [JsonProperty("cancel_url")] public string CancelUrl;
    }

    /// <summary>Answer of <c>POST /checkout/stripe</c>: <c>status = ok</c>, the hosted Checkout page and its session id.</summary>
    public sealed class StripeCheckoutResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        [JsonProperty("session_id")] public string SessionId;
        [JsonProperty("url")] public string Url;
    }

    /// <summary>Body of <c>POST /verify/stripe</c>: confirms a Checkout Session after the redirect, with what the
    /// client knows about the purchase (for GA4, attribution integrations and A/B test results).</summary>
    public sealed class StripeSessionRequest
    {
        [JsonProperty("app_id")] public string AppId;
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("session_id")] public string SessionId;
        [JsonProperty("app_instance_id", NullValueHandling = NullValueHandling.Ignore)] public string AppInstanceId;
        [JsonProperty("attribution", NullValueHandling = NullValueHandling.Ignore)] public Dictionary<string, string> Attribution;
        [JsonProperty("placement", NullValueHandling = NullValueHandling.Ignore)] public string Placement;
        [JsonProperty("variant_id", NullValueHandling = NullValueHandling.Ignore)] public string VariantId;
        [JsonProperty("test_id", NullValueHandling = NullValueHandling.Ignore)] public string TestId;
        [JsonProperty("paywall_id", NullValueHandling = NullValueHandling.Ignore)] public string PaywallId;
    }

    /// <summary>
    /// Stripe routes of the billing server. Implemented by <see cref="BillingApiClient"/>; used by
    /// <see cref="StripeStoreAdapter"/>.
    /// </summary>
    public interface IStripeCheckoutApi
    {
        /// <summary><c>POST /checkout/stripe</c>.</summary>
        Task<ApiResult<StripeCheckoutResponse>> CreateStripeCheckoutAsync(StripeCheckoutRequest request, CancellationToken ct = default);

        /// <summary><c>POST /verify/stripe</c>: answers like <c>/verify/{store}</c> (<see cref="VerifyStatus"/>);
        /// <c>pending_or_canceled</c> while the session is not paid.</summary>
        Task<ApiResult<VerifyResponse>> VerifyStripeSessionAsync(StripeSessionRequest request, CancellationToken ct = default);
    }

    public sealed partial class BillingApiClient : IStripeCheckoutApi
    {
        public Task<ApiResult<StripeCheckoutResponse>> CreateStripeCheckoutAsync(StripeCheckoutRequest request, CancellationToken ct = default) =>
            PostAsync<StripeCheckoutResponse>("/checkout/stripe", request, ct);

        public Task<ApiResult<VerifyResponse>> VerifyStripeSessionAsync(StripeSessionRequest request, CancellationToken ct = default) =>
            PostAsync<VerifyResponse>("/verify/stripe", request, ct);
    }
}
