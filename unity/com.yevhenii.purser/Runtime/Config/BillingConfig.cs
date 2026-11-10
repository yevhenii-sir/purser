using System;
using System.Collections.Generic;
using UnityEngine;

namespace Purser
{
    public enum EditorVerificationMode
    {
        /// <summary>In the Editor (Unity IAP fake store) purchases are confirmed locally without the server.</summary>
        SkipServer,
        /// <summary>Send Editor purchases to the server as well (they will be rejected as invalid).</summary>
        CallServer,
    }

    public enum BillingLogLevel
    {
        None,
        Error,
        Warning,
        Info,
        Verbose,
    }

    /// <summary>
    /// Billing configuration asset. Create it with Assets → Create → Purser → Config
    /// (or Tools → Purser → Create Config) and assign it to <see cref="BillingBootstrap"/>
    /// or pass it to <see cref="Billing.Create"/>.
    /// </summary>
    [CreateAssetMenu(menuName = "Purser/Config", fileName = "BillingConfig")]
    public sealed class BillingConfig : ScriptableObject
    {
        /// <summary>Marks the part of <see cref="DefaultServerUrl"/> that must be replaced with your database name.</summary>
        public const string ServerUrlPlaceholder = "YOUR-BILLING-DB";

        public const string DefaultServerUrl = "https://maincloud.spacetimedb.com/v1/database/" + ServerUrlPlaceholder + "/route";

        [Header("Server")]
        [Tooltip("Base URL of the billing HTTP API: https://<host>/v1/database/<db>/route")]
        public string serverBaseUrl = DefaultServerUrl;

        [Tooltip("Google Play package name registered on the billing server. Empty = Application.identifier.")]
        public string androidAppId;

        [Tooltip("App Store bundle id registered on the billing server. Empty = Application.identifier.")]
        public string iosAppId;

        [Tooltip("App id of the Amazon Appstore build on the billing server (e.g. com.example.mygame.amazon " +
                 "when the Google Play build uses the package name). Empty = Application.identifier.")]
        public string amazonAppId;

        [Tooltip("App id of the Steam build on the billing server. Empty = Application.identifier.")]
        public string steamAppId;

        [Tooltip("App id of the web build on the billing server. Empty = Application.identifier.")]
        public string webAppId;

        [Tooltip("Store of Android builds. Auto: the Amazon Appstore when it installed the app, otherwise Google Play. " +
                 "Amazon needs an AmazonStoreAdapter in BillingDependencies.Store (Unity IAP 5 has no Amazon store).")]
        public AndroidStore androidStore = AndroidStore.Auto;

        [Min(1)]
        public int requestTimeoutSeconds = 10;

        [Tooltip("Delays between verification retries when the server is unreachable. " +
                 "After the last one the purchase stays pending and is retried on the next launch.")]
        public int[] retryDelaysSeconds = { 5, 30, 120, 600 };

        [Header("Products")]
        public List<BillingProductConfig> products = new List<BillingProductConfig>();

        [Header("Behaviour")]
        [Tooltip("Bind purchases to the player: Google obfuscatedAccountId / Apple appAccountToken. " +
                 "The server then refuses to credit the purchase to another player.")]
        public bool bindPurchasesToUser = true;

        [Tooltip("Load entitlements from the server right after initialization.")]
        public bool refreshEntitlementsOnInit = true;

        [Tooltip("iOS: find out which Apple Ads campaign brought the install (AdServices, asked once per install) " +
                 "and send its ids with purchases. They reach the webhook and Cloud Storage exports.")]
        public bool collectAppleAdsAttribution;

        public EditorVerificationMode editorMode = EditorVerificationMode.SkipServer;

        public BillingLogLevel logLevel = BillingLogLevel.Warning;

        /// <summary>Creates a config in code (no asset).</summary>
        public static BillingConfig Create(string serverBaseUrl, params BillingProductConfig[] products)
        {
            var config = CreateInstance<BillingConfig>();
            config.serverBaseUrl = serverBaseUrl;
            config.products = new List<BillingProductConfig>(products);
            return config;
        }

        /// <summary>
        /// Applies the "Unity config" JSON shown by the admin panel's Add game wizard:
        /// <c>{"serverBaseUrl", "androidAppId", "iosAppId", "amazonAppId", "steamAppId", "webAppId",
        /// "products": [{"id", "type", "googleId", "appleId", "amazonId"}]}</c>.
        /// Missing fields keep their value; <c>products</c>, when present, replaces the list.
        /// <c>type</c> accepts <c>Consumable</c> / <c>NonConsumable</c> / <c>Subscription</c> or the server's
        /// <c>consumable</c> / <c>non_consumable</c> / <c>subscription</c>.
        /// </summary>
        public void ApplyJson(string json)
        {
            var o = Newtonsoft.Json.Linq.JObject.Parse(json);
            string Str(Newtonsoft.Json.Linq.JToken t, string name) => (string)t[name];

            serverBaseUrl = Str(o, "serverBaseUrl")?.TrimEnd('/') ?? serverBaseUrl;
            androidAppId = Str(o, "androidAppId") ?? androidAppId;
            iosAppId = Str(o, "iosAppId") ?? iosAppId;
            amazonAppId = Str(o, "amazonAppId") ?? amazonAppId;
            steamAppId = Str(o, "steamAppId") ?? steamAppId;
            webAppId = Str(o, "webAppId") ?? webAppId;
            if (!(o["products"] is Newtonsoft.Json.Linq.JArray items))
                return;
            var parsed = new List<BillingProductConfig>();
            foreach (var item in items)
            {
                var id = Str(item, "id");
                if (string.IsNullOrEmpty(id))
                    continue;
                var type = (Str(item, "type") ?? "").Replace("_", "");
                if (!Enum.TryParse(type, true, out BillingProductType parsedType))
                    throw new FormatException($"Product '{id}': unknown type '{Str(item, "type")}'.");
                parsed.Add(new BillingProductConfig(id, parsedType, Str(item, "googleId"), Str(item, "appleId"), Str(item, "amazonId")));
            }
            products = parsed;
        }

        public string AppIdFor(BillingPlatform platform)
        {
            string overrideId;
            switch (platform)
            {
                case BillingPlatform.Apple: overrideId = iosAppId; break;
                case BillingPlatform.Amazon: overrideId = amazonAppId; break;
                case BillingPlatform.Steam: overrideId = steamAppId; break;
                case BillingPlatform.Web: overrideId = webAppId; break;
                default: overrideId = androidAppId; break;
            }
            return string.IsNullOrEmpty(overrideId) ? Application.identifier : overrideId;
        }

        public BillingProductConfig FindById(string id)
        {
            return products.Find(p => p != null && p.id == id);
        }

        public BillingProductConfig FindByStoreId(string storeId)
        {
            return products.Find(p => p != null && p.Matches(storeId));
        }

        /// <summary>Returns human readable problems; empty when the config is usable.</summary>
        public List<string> Validate()
        {
            var problems = new List<string>();
            if (string.IsNullOrWhiteSpace(serverBaseUrl))
                problems.Add("Server base URL is empty.");
            else if (!serverBaseUrl.StartsWith("https://", StringComparison.OrdinalIgnoreCase))
                problems.Add("Server base URL must use https://.");
            else if (serverBaseUrl.EndsWith("/"))
                problems.Add("Server base URL must not end with '/'.");
            else if (serverBaseUrl.Contains(ServerUrlPlaceholder))
                problems.Add($"Set the server base URL to your billing database (replace {ServerUrlPlaceholder}).");

            if (requestTimeoutSeconds < 1)
                problems.Add("Request timeout must be at least 1 second.");
            if (retryDelaysSeconds == null)
                problems.Add("Retry delays are not set.");
            else if (Array.Exists(retryDelaysSeconds, d => d < 0))
                problems.Add("Retry delays must not be negative.");

            if (products == null || products.Count == 0)
                problems.Add("No products configured.");
            else
            {
                var seen = new HashSet<string>();
                for (var i = 0; i < products.Count; i++)
                {
                    var p = products[i];
                    if (p == null || string.IsNullOrWhiteSpace(p.id))
                        problems.Add($"Product #{i} has no id.");
                    else if (!seen.Add(p.id))
                        problems.Add($"Duplicate product id '{p.id}'.");
                }
            }
            return problems;
        }
    }
}
