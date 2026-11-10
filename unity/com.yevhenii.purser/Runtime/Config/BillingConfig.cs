using System;
using System.Collections.Generic;
using System.Linq;
using Newtonsoft.Json.Linq;
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

    /// <summary>One build of the app on the billing server: its store and app id there.</summary>
    [Serializable]
    public sealed class BillingBuild
    {
        [Tooltip("Store id of the billing server: google, apple, amazon, steam, stripe, xsolla (see BillingStores).")]
        public string store;

        [Tooltip("App id of the build on the server: Android package, iOS bundle id, or any unique id " +
                 "(e.g. com.example.mygame.web).")]
        public string appId;

        public BillingBuild() { }

        public BillingBuild(string store, string appId)
        {
            this.store = store;
            this.appId = appId;
        }
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

        [Tooltip("The app's builds on the billing server: one per store (google, apple, amazon, steam, stripe, xsolla) " +
                 "with its app id. A store without a row uses Application.identifier.")]
        public List<BillingBuild> builds = new List<BillingBuild>();

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
        /// Applies the config JSON copied from the admin panel (or from <see cref="ToJson"/>):
        /// <c>{"serverBaseUrl", "builds": [{"store", "appId"}], "products": [{"id", "type", "storeIds": {"apple": "..."}}]}</c>.
        /// Missing fields keep their value; <c>builds</c> and <c>products</c>, when present, replace the lists.
        /// <c>type</c> accepts <c>Consumable</c> / <c>NonConsumable</c> / <c>Subscription</c> or the server's
        /// <c>consumable</c> / <c>non_consumable</c> / <c>subscription</c>.
        /// </summary>
        /// <exception cref="FormatException">The JSON is not a Purser config.</exception>
        public void ApplyJson(string json)
        {
            JObject o;
            try
            {
                o = JObject.Parse(json ?? "");
            }
            catch (Newtonsoft.Json.JsonException e)
            {
                throw new FormatException("not JSON: " + e.Message);
            }
            foreach (var legacy in new[] { "androidAppId", "iosAppId", "amazonAppId", "steamAppId", "webAppId" })
                if (o[legacy] != null)
                    throw new FormatException($"'{legacy}' is from an older Purser: copy the config again from the admin panel");

            var parsedBuilds = o["builds"] is JArray b ? ParseBuilds(b) : null;
            var parsedProducts = o["products"] is JArray p ? ParseProducts(p) : null;
            serverBaseUrl = ((string)o["serverBaseUrl"])?.TrimEnd('/') ?? serverBaseUrl;
            if (parsedBuilds != null)
                builds = parsedBuilds;
            if (parsedProducts != null)
                products = parsedProducts;
        }

        private static List<BillingBuild> ParseBuilds(JArray items)
        {
            var parsed = new List<BillingBuild>();
            foreach (var item in items)
            {
                var store = (string)item["store"];
                var appId = (string)item["appId"];
                if (string.IsNullOrEmpty(store) || string.IsNullOrEmpty(appId))
                    throw new FormatException("every build needs a store and an appId");
                parsed.Add(new BillingBuild(store, appId));
            }
            return parsed;
        }

        private static List<BillingProductConfig> ParseProducts(JArray items)
        {
            var parsed = new List<BillingProductConfig>();
            foreach (var item in items)
            {
                var id = (string)item["id"];
                if (string.IsNullOrEmpty(id))
                    continue;
                var type = ((string)item["type"] ?? "").Replace("_", "");
                if (!Enum.TryParse(type, true, out BillingProductType parsedType))
                    throw new FormatException($"product '{id}': unknown type '{(string)item["type"]}'");
                var product = new BillingProductConfig(id, parsedType);
                if (item["storeIds"] is JObject storeIds)
                    foreach (var kv in storeIds)
                        if (!string.IsNullOrEmpty((string)kv.Value))
                            product.WithStoreId(kv.Key, (string)kv.Value);
                parsed.Add(product);
            }
            return parsed;
        }

        /// <summary>The config as JSON for the admin panel ("Paste from Unity"); the inverse of <see cref="ApplyJson"/>.</summary>
        public string ToJson()
        {
            var o = new JObject
            {
                ["serverBaseUrl"] = serverBaseUrl,
                ["builds"] = new JArray((builds ?? new List<BillingBuild>())
                    .Where(b => b != null && !string.IsNullOrEmpty(b.store))
                    .Select(b => new JObject { ["store"] = b.store, ["appId"] = b.appId ?? "" })),
                ["products"] = new JArray((products ?? new List<BillingProductConfig>())
                    .Where(p => p != null && !string.IsNullOrEmpty(p.id))
                    .Select(p =>
                    {
                        var item = new JObject { ["id"] = p.id, ["type"] = p.type.ToString() };
                        var overrides = (p.storeIds ?? new List<StoreProductId>())
                            .Where(s => s != null && !string.IsNullOrEmpty(s.store) && !string.IsNullOrEmpty(s.id) && s.id != p.id)
                            .GroupBy(s => s.store)
                            .ToList();
                        if (overrides.Count > 0)
                            item["storeIds"] = new JObject(overrides.Select(g => new JProperty(g.Key, g.First().id)));
                        return item;
                    })),
            };
            return o.ToString(Newtonsoft.Json.Formatting.Indented);
        }

        /// <summary>App id of the build in a store (<see cref="BillingStores"/>); <c>Application.identifier</c> when the
        /// config has no build for it.</summary>
        public string AppIdFor(string store)
        {
            var build = builds?.FirstOrDefault(b => b != null && b.store == store && !string.IsNullOrEmpty(b.appId));
            return build?.appId ?? Application.identifier;
        }

        /// <summary>Adds or replaces the build of a store (fluent, for configs built in code).</summary>
        public BillingConfig WithBuild(string store, string appId)
        {
            builds = builds ?? new List<BillingBuild>();
            builds.RemoveAll(b => b?.store == store);
            builds.Add(new BillingBuild(store, appId));
            return this;
        }

        /// <summary>The product with this game id, or null.</summary>
        public BillingProductConfig FindById(string id)
        {
            return products.Find(p => p != null && p.id == id);
        }

        /// <summary>The product with this id in any store, or null.</summary>
        public BillingProductConfig FindByStoreId(string storeId)
        {
            return string.IsNullOrEmpty(storeId) ? null : products.Find(p => p != null && p.Matches(storeId));
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

            var stores = new HashSet<string>();
            foreach (var b in builds ?? new List<BillingBuild>())
            {
                if (b == null || string.IsNullOrWhiteSpace(b.store))
                    problems.Add("A build has no store.");
                else if (!stores.Add(b.store))
                    problems.Add($"Two builds for the store '{b.store}': keep one.");
                else if (string.IsNullOrWhiteSpace(b.appId))
                    problems.Add($"The {b.store} build has no app id.");
                else if (!BillingStores.All.Contains(b.store))
                    problems.Add($"Unknown store '{b.store}' (known: {string.Join(", ", BillingStores.All)}).");
            }

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
