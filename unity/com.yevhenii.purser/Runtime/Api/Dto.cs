using System.Collections.Generic;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Purser
{
    /// <summary>Values of <c>status</c> in server responses.</summary>
    public static class VerifyStatus
    {
        public const string Success = "success";
        public const string Pending = "pending";
        public const string PendingOrCanceled = "pending_or_canceled";
        public const string Invalid = "invalid";
        public const string Forbidden = "forbidden";
        public const string Expired = "expired";
        public const string Revoked = "revoked";
        public const string Error = "error";
    }

    /// <summary>Values of <c>delivery</c> in <c>/verify</c> responses: who grants the reward.</summary>
    public static class VerifyDelivery
    {
        /// <summary>The server delivers the reward to the game backend and finishes Google purchases itself.</summary>
        public const string Server = "server";

        /// <summary>No game backend (<c>delivery = none</c>): the game grants the reward in
        /// <see cref="IBillingService.PurchaseSucceeded"/>, then the package finishes the purchase in the store.</summary>
        public const string Client = "client";
    }

    /// <summary>Body of <c>POST /verify/{store}</c> (<c>google</c>, <c>apple</c>, <c>amazon</c>, ...).</summary>
    public sealed class VerifyRequest
    {
        [JsonProperty("bundle_id")] public string BundleId;
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("product_id")] public string ProductId;
        /// <summary>Google purchase token.</summary>
        [JsonProperty("purchase_token", NullValueHandling = NullValueHandling.Ignore)] public string PurchaseToken;
        /// <summary>Apple transactionId, Amazon receiptId (required) / Google orderId (optional).</summary>
        [JsonProperty("order_id", NullValueHandling = NullValueHandling.Ignore)] public string OrderId;
        /// <summary>The player's id in the store (Amazon userId).</summary>
        [JsonProperty("store_user_id", NullValueHandling = NullValueHandling.Ignore)] public string StoreUserId;
        [JsonProperty("app_instance_id", NullValueHandling = NullValueHandling.Ignore)] public string AppInstanceId;
        [JsonProperty("price_micros")] public long PriceMicros;
        [JsonProperty("currency", NullValueHandling = NullValueHandling.Ignore)] public string Currency;
        /// <summary>Device ids for attribution integrations (<see cref="AttributionKeys"/>).</summary>
        [JsonProperty("attribution", NullValueHandling = NullValueHandling.Ignore)] public Dictionary<string, string> Attribution;
        /// <summary>Paywall the purchase was made from (<see cref="Paywall"/>), for A/B test results.</summary>
        [JsonProperty("placement", NullValueHandling = NullValueHandling.Ignore)] public string Placement;
        [JsonProperty("variant_id", NullValueHandling = NullValueHandling.Ignore)] public string VariantId;
        [JsonProperty("test_id", NullValueHandling = NullValueHandling.Ignore)] public string TestId;
        [JsonProperty("paywall_id", NullValueHandling = NullValueHandling.Ignore)] public string PaywallId;
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    /// <summary>The device a request comes from: model, OS, app and SDK version.</summary>
    public sealed class ClientInfoDto
    {
        [JsonProperty("device")] public string Device;
        [JsonProperty("os")] public string Os;
        [JsonProperty("app_version")] public string AppVersion;
        [JsonProperty("sdk_version")] public string SdkVersion;
    }

    /// <summary>Body of <c>POST /attributes</c>: custom attributes of a player; <c>null</c> removes a key.</summary>
    public sealed class AttributesRequest
    {
        [JsonProperty("app_id")] public string AppId;
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("attributes", NullValueHandling = NullValueHandling.Include)] public Dictionary<string, object> Attributes;
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    /// <summary>Answer of <c>POST /attributes</c>: the player's attributes after the update.</summary>
    public sealed class AttributesResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        [JsonProperty("attributes")] public Dictionary<string, string> Attributes;
    }

    public sealed class VerifyResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
        [JsonProperty("transaction_id")] public string TransactionId;
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("is_sub")] public bool IsSubscription;
        [JsonProperty("expires_date_ms")] public long ExpiresDateMs;
        [JsonProperty("environment")] public string Environment;
        /// <summary><see cref="VerifyDelivery"/>; treated as <see cref="VerifyDelivery.Server"/> when absent.</summary>
        [JsonProperty("delivery")] public string Delivery;
    }

    public sealed class RestoreItem
    {
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("purchase_token", NullValueHandling = NullValueHandling.Ignore)] public string PurchaseToken;
        [JsonProperty("order_id", NullValueHandling = NullValueHandling.Ignore)] public string OrderId;
        [JsonProperty("store_user_id", NullValueHandling = NullValueHandling.Ignore)] public string StoreUserId;
    }

    /// <summary>Body of <c>POST /restore</c> (at most 20 purchases per call).</summary>
    public sealed class RestoreRequest
    {
        public const int MaxItems = 20;

        [JsonProperty("app_id")] public string AppId;
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("app_instance_id", NullValueHandling = NullValueHandling.Ignore)] public string AppInstanceId;
        [JsonProperty("attribution", NullValueHandling = NullValueHandling.Ignore)] public Dictionary<string, string> Attribution;
        [JsonProperty("purchases")] public List<RestoreItem> Purchases = new List<RestoreItem>();
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    public sealed class RestoreItemResult
    {
        [JsonProperty("product_id")] public string ProductId;
        [JsonProperty("http_status")] public int HttpStatus;
        [JsonProperty("status")] public string Status;
        [JsonProperty("message")] public string Message;
    }

    public sealed class RestoreResponse
    {
        [JsonProperty("results")] public List<RestoreItemResult> Results;
        [JsonProperty("entitlements")] public EntitlementsDto Entitlements;
    }

    /// <summary>Body of <c>POST /entitlements</c>.</summary>
    public sealed class EntitlementsRequest
    {
        [JsonProperty("app_id")] public string AppId;
        [JsonProperty("user_id")] public string UserId;
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    public sealed class EntitlementsDto
    {
        [JsonProperty("user_id")] public string UserId;
        [JsonProperty("game_key")] public string GameKey;
        [JsonProperty("subscriptions")] public Dictionary<string, long> Subscriptions;
        [JsonProperty("non_consumables")] public List<string> NonConsumables;
        [JsonProperty("server_time_ms")] public long ServerTimeMs;
    }

    /// <summary>Body of <c>POST /paywall</c>.</summary>
    public sealed class PaywallRequest
    {
        [JsonProperty("app_id")] public string AppId;
        [JsonProperty("placement")] public string Placement;
        /// <summary>Assigns the player to a running A/B test; without it the default variant is served.</summary>
        [JsonProperty("user_id", NullValueHandling = NullValueHandling.Ignore)] public string UserId;
        /// <summary>The device, shown on the admin page's player profile (filled by <see cref="BillingService"/>).</summary>
        [JsonProperty("client", NullValueHandling = NullValueHandling.Ignore)] public ClientInfoDto Client;
    }

    public sealed class PaywallProductDto
    {
        /// <summary>Catalog product id on the server.</summary>
        [JsonProperty("id")] public string Id;
        /// <summary>Product id in this build's store.</summary>
        [JsonProperty("store_id")] public string StoreId;
        /// <summary><c>consumable</c> | <c>non_consumable</c> | <c>subscription</c>.</summary>
        [JsonProperty("kind")] public string Kind;
    }

    /// <summary>Answer of <c>POST /paywall</c>; errors carry <see cref="Code"/> (e.g. <c>ERR_PLACEMENT_NOT_FOUND</c>).</summary>
    public sealed class PaywallResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("code")] public string Code;
        [JsonProperty("message")] public string Message;
        [JsonProperty("placement")] public string Placement;
        [JsonProperty("paywall_id")] public string PaywallId;
        [JsonProperty("paywall_name")] public string PaywallName;
        [JsonProperty("variant_id")] public string VariantId;
        [JsonProperty("test_id")] public string TestId;
        [JsonProperty("products")] public List<PaywallProductDto> Products;
        [JsonProperty("remote_config")] public JToken RemoteConfig;
        [JsonProperty("updated_at_ms")] public long UpdatedAtMs;
    }

    /// <summary>Body of <c>POST /paywall/event</c>.</summary>
    public sealed class PaywallEventRequest
    {
        public const string View = "view";

        [JsonProperty("app_id")] public string AppId;
        [JsonProperty("placement")] public string Placement;
        [JsonProperty("variant_id")] public string VariantId;
        [JsonProperty("test_id", NullValueHandling = NullValueHandling.Ignore)] public string TestId;
        [JsonProperty("user_id", NullValueHandling = NullValueHandling.Ignore)] public string UserId;
        [JsonProperty("event")] public string Event = View;
    }

    /// <summary>Generic <c>{status, code, message}</c> answer.</summary>
    public sealed class StatusResponse
    {
        [JsonProperty("status")] public string Status;
        [JsonProperty("code")] public string Code;
        [JsonProperty("message")] public string Message;
    }
}
