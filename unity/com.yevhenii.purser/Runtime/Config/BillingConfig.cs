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

        /// <summary>Server URL of a new config; replace <see cref="ServerUrlPlaceholder"/>.</summary>
        public const string DefaultServerUrl = "https://maincloud.spacetimedb.com/v1/database/" + ServerUrlPlaceholder + "/route";

        /// <summary>Default <see cref="requestTimeoutSeconds"/>: the server waits for the store's API, which can be slow.</summary>
        public const int DefaultRequestTimeoutSeconds = 30;

        /// <summary>Default <see cref="entitlementsMaxAgeDays"/>.</summary>
        public const int DefaultEntitlementsMaxAgeDays = 7;

        [Header("Server")]
        [Tooltip("Base URL of the billing HTTP API: https://<host>/v1/database/<db>/route")]
        public string serverBaseUrl = DefaultServerUrl;

        [Tooltip("The app's builds on the billing server: one per store (google, apple, amazon, steam, stripe, xsolla) " +
                 "with its app id. A store without a row uses Application.identifier.")]
        public List<BillingBuild> builds = new List<BillingBuild>();

        [Tooltip("Store of Android builds. Auto: the Amazon Appstore when it installed the app, otherwise Google Play. " +
                 "Amazon needs an AmazonStoreAdapter in BillingDependencies.Store (Unity IAP 5 has no Amazon store).")]
        public AndroidStore androidStore = AndroidStore.Auto;

        [Tooltip("Timeout of one request to the billing server. The server may wait for a slow store API (it allows " +
                 "itself up to about 10 s per store call), so keep it well above that.")]
        [Min(1)]
        public int requestTimeoutSeconds = DefaultRequestTimeoutSeconds;

        [Tooltip("Base64 Ed25519 public key of signed server responses (the build's response signing key on the admin " +
                 "page). When set, /entitlements and /restore answers without a valid X-Purser-Signature are rejected " +
                 "and the cached customer info is kept. Empty = off.")]
        public string responseSigningPublicKey = "";

        [Tooltip("Delays between verification retries when the server is unreachable. " +
                 "After the last one the purchase stays pending and is retried on the next launch.")]
        public int[] retryDelaysSeconds = { 5, 30, 120, 600 };

        [Tooltip("When the server cannot be reached for this many seconds after a purchase, grant it anyway and verify " +
                 "it later (PurchaseResult.Provisional). Only for games that grant purchases on the client (delivery " +
                 "none). Products with strictVerification always wait. 0 = always wait for the server.")]
        [Min(0)]
        public int failOpenAfterSeconds;

        [Header("Products")]
        public List<BillingProductConfig> products = new List<BillingProductConfig>();

        [Header("Behaviour")]
        [Tooltip("Bind purchases to the player: Google obfuscatedAccountId / Apple appAccountToken. " +
                 "The server then refuses to credit the purchase to another player.")]
        public bool bindPurchasesToUser = true;

        [Tooltip("Load the customer info from the server right after initialization.")]
        public bool refreshEntitlementsOnInit = true;

        [Tooltip("IsActive, HasAccess and GetExpiration trust subscriptions and expiring access from customer info " +
                 "received at most this many days ago (device clock), e.g. the offline cache; older ones count again " +
                 "after the next refresh from the server. Non-consumables and lifetime access are not affected. 0 = no limit.")]
        [Min(0)]
        public int entitlementsMaxAgeDays = DefaultEntitlementsMaxAgeDays;

        [Tooltip("iOS: find out which Apple Ads campaign brought the install (AdServices, asked once per install) " +
                 "and send its ids with purchases. They reach the webhook and Cloud Storage exports.")]
        public bool collectAppleAdsAttribution;

        public EditorVerificationMode editorMode = EditorVerificationMode.SkipServer;

        public BillingLogLevel logLevel = BillingLogLevel.Warning;

        [Header("Profiles")]
        [Tooltip("Environments (dev, staging, prod): each names its server and what differs there (builds, product ids). " +
                 "Everything else comes from the fields above. Empty = no profiles.")]
        public List<BillingProfile> profiles = new List<BillingProfile>();

        [Tooltip("Profile used in the Editor (play mode, Editor tests). Empty = the fields above.")]
        public string editorProfile = "";

        [Tooltip("Profile of Development Builds. Empty = the fields above.")]
        public string developmentProfile = "";

        [Tooltip("Profile of release builds. Empty = the fields above.")]
        public string releaseProfile = "";

        /// <summary>The profile <see cref="UseProfile"/> chose; null = the rules.</summary>
        [NonSerialized] private string m_RuntimeProfile;
        /// <summary>This is a copy with its profile applied (<see cref="Resolved"/>).</summary>
        [NonSerialized] private bool m_IsResolved;
        [NonSerialized] private string m_ResolvedProfile = "";

        /// <summary>
        /// The profile this config is used with: on a copy from <see cref="Resolved"/>, the one applied; otherwise the
        /// one <see cref="Resolved"/> would apply (<see cref="UseProfile"/>, else <see cref="editorProfile"/> in the
        /// Editor, <see cref="developmentProfile"/> in Development Builds, <see cref="releaseProfile"/>). Empty: none.
        /// </summary>
        public string ProfileName => m_IsResolved
            ? m_ResolvedProfile
            : SelectProfile(m_RuntimeProfile, Application.isEditor, Debug.isDebugBuild, editorProfile, developmentProfile, releaseProfile);

        /// <summary>
        /// Picks the profile at run time, over the Editor / Development / release rules (a QA menu that switches to
        /// staging). Call it before creating the billing service and the store adapters; <c>""</c> uses no profile,
        /// null goes back to the rules. Not saved in the asset.
        /// </summary>
        public void UseProfile(string profileName) => m_RuntimeProfile = profileName;

        /// <summary>The profile with this name, or null.</summary>
        public BillingProfile FindProfile(string profileName) =>
            string.IsNullOrEmpty(profileName) ? null : profiles?.Find(p => p != null && p.name == profileName);

        /// <summary>
        /// The config to run with: a copy with the <see cref="ProfileName"/> profile applied, or this config when no
        /// profile is chosen. The billing service, the API client and the store adapters call it themselves, so pass
        /// them the asset. The asset never changes.
        /// </summary>
        /// <exception cref="BillingException">The chosen profile does not exist.</exception>
        public BillingConfig Resolved()
        {
            if (m_IsResolved)
                return this;
            var profileName = ProfileName;
            if (string.IsNullOrEmpty(profileName))
                return this;
            var profile = FindProfile(profileName);
            if (profile == null)
                throw new BillingException($"Billing config {name} has no profile '{profileName}' " +
                                          $"(profiles: {string.Join(", ", (profiles ?? new List<BillingProfile>()).Where(p => p != null).Select(p => p.name))})");
            return WithProfile(profile);
        }

        /// <summary>A copy of this config with <paramref name="profile"/> applied: its server, signing key, builds and
        /// product ids replace the config's where it sets them.</summary>
        public BillingConfig WithProfile(BillingProfile profile)
        {
            if (profile == null)
                throw new ArgumentNullException(nameof(profile));
            var copy = Clone();
            copy.name = $"{name} ({profile.name})";
            if (!string.IsNullOrWhiteSpace(profile.serverBaseUrl))
                copy.serverBaseUrl = profile.serverBaseUrl.Trim();
            if (!string.IsNullOrWhiteSpace(profile.responseSigningPublicKey))
                copy.responseSigningPublicKey = profile.responseSigningPublicKey.Trim();
            foreach (var b in profile.builds ?? new List<BillingBuild>())
                if (b != null && !string.IsNullOrEmpty(b.store) && !string.IsNullOrEmpty(b.appId))
                    copy.WithBuild(b.store, b.appId);
            foreach (var o in profile.productIds ?? new List<ProductIdOverride>())
                if (o != null && !string.IsNullOrEmpty(o.store) && !string.IsNullOrEmpty(o.id))
                    copy.FindById(o.product)?.WithStoreId(o.store, o.id);
            copy.m_IsResolved = true;
            copy.m_ResolvedProfile = profile.name ?? "";
            return copy;
        }

        /// <summary>The Editor / Development / release rule (<paramref name="runtime"/>: <see cref="UseProfile"/>).</summary>
        internal static string SelectProfile(string runtime, bool isEditor, bool isDebugBuild, string editor, string development,
            string release)
        {
            var chosen = runtime ?? (isEditor ? editor : isDebugBuild ? development : release);
            return chosen?.Trim() ?? "";
        }

        /// <summary>A deep copy of the settings, builds and products (profiles are shared).</summary>
        internal BillingConfig Clone()
        {
            var c = CreateInstance<BillingConfig>();
            c.name = name;
            c.serverBaseUrl = serverBaseUrl;
            c.builds = (builds ?? new List<BillingBuild>()).Select(b => b == null ? null : new BillingBuild(b.store, b.appId)).ToList();
            c.androidStore = androidStore;
            c.requestTimeoutSeconds = requestTimeoutSeconds;
            c.responseSigningPublicKey = responseSigningPublicKey;
            c.retryDelaysSeconds = (int[])retryDelaysSeconds?.Clone();
            c.failOpenAfterSeconds = failOpenAfterSeconds;
            c.products = (products ?? new List<BillingProductConfig>()).Select(p => p?.Clone()).ToList();
            c.bindPurchasesToUser = bindPurchasesToUser;
            c.refreshEntitlementsOnInit = refreshEntitlementsOnInit;
            c.entitlementsMaxAgeDays = entitlementsMaxAgeDays;
            c.collectAppleAdsAttribution = collectAppleAdsAttribution;
            c.editorMode = editorMode;
            c.logLevel = logLevel;
            c.profiles = profiles;
            c.editorProfile = editorProfile;
            c.developmentProfile = developmentProfile;
            c.releaseProfile = releaseProfile;
            c.m_RuntimeProfile = m_RuntimeProfile;
            return c;
        }

        /// <summary>Creates a config in code (no asset).</summary>
        public static BillingConfig Create(string serverBaseUrl, params BillingProductConfig[] products)
        {
            var config = CreateInstance<BillingConfig>();
            config.serverBaseUrl = serverBaseUrl;
            config.products = new List<BillingProductConfig>(products);
            return config;
        }

        /// <summary>
        /// What <see cref="MergeJson"/> would change, without changing anything.
        /// </summary>
        /// <exception cref="FormatException">The JSON is not a Purser config.</exception>
        public List<ConfigChange> PreviewJson(string json, ConfigMergeMode mode = ConfigMergeMode.Additive) => Merge(json, mode, false);

        /// <summary>
        /// Merges the config JSON copied from the admin panel (or from <see cref="ToJson"/>) into this config and returns
        /// what changed: <c>{"serverBaseUrl", "builds": [{"store", "appId"}], "products": [{"id", "type",
        /// "storeIds": {"apple": "..."}, "price": 0.99, "currency": "USD", "period": "P1M", "access": ["premium"]}]}</c>
        /// (<c>price</c> / <c>currency</c>: the fallback price; <c>period</c>: the fallback subscription period;
        /// <c>access</c>: the access levels, replaced when the JSON lists them). A build is matched by its store, a product by its id or any of its store ids.
        /// <see cref="ConfigMergeMode.Additive"/> only adds and updates: builds and products only in this config stay.
        /// <see cref="ConfigMergeMode.Mirror"/> also removes them and the store ids the JSON does not list, so the
        /// config matches the admin panel (fallback prices are kept where the JSON has none). <c>type</c> accepts
        /// <c>Consumable</c> / <c>NonConsumable</c> / <c>Subscription</c> or the server's <c>consumable</c> /
        /// <c>non_consumable</c> / <c>subscription</c>. An invalid JSON changes nothing.
        /// </summary>
        /// <exception cref="FormatException">The JSON is not a Purser config.</exception>
        public List<ConfigChange> MergeJson(string json, ConfigMergeMode mode = ConfigMergeMode.Additive) => Merge(json, mode, true);

        /// <summary>What <see cref="MergeJsonIntoProfile"/> would change, without changing anything.</summary>
        /// <exception cref="FormatException">The JSON is not a Purser config.</exception>
        /// <exception cref="ArgumentException">No profile with this name.</exception>
        public List<ConfigChange> PreviewJsonForProfile(string json, string profileName, ConfigMergeMode mode = ConfigMergeMode.Additive) =>
            MergeIntoProfile(json, profileName, mode, false);

        /// <summary>
        /// Merges the admin panel's config JSON of one environment (its own database) into a profile: the server URL,
        /// the builds and the store ids that differ from this config's become the profile's; the catalog (new
        /// products, types, access levels, fallback prices; with <see cref="ConfigMergeMode.Mirror"/> removals too)
        /// goes to this config, which every profile shares. A build or product id equal to the config's needs no
        /// profile row and loses it.
        /// </summary>
        /// <exception cref="FormatException">The JSON is not a Purser config.</exception>
        /// <exception cref="ArgumentException">No profile with this name.</exception>
        public List<ConfigChange> MergeJsonIntoProfile(string json, string profileName, ConfigMergeMode mode = ConfigMergeMode.Additive) =>
            MergeIntoProfile(json, profileName, mode, true);

        private List<ConfigChange> MergeIntoProfile(string json, string profileName, ConfigMergeMode mode, bool apply)
        {
            var profile = FindProfile(profileName) ?? throw new ArgumentException($"no profile '{profileName}'", nameof(profileName));
            var o = ParseJson(json);
            var incomingBuilds = o["builds"] is JArray b ? ParseBuilds(b) : new List<BillingBuild>();
            var items = o["products"] as JArray;
            if (items != null)
                ParseProducts(items); // A bad product fails before anything changes.
            var effective = WithProfile(profile);
            var label = $"Profile {profile.name}: ";
            var changes = new List<ConfigChange>();

            var url = ((string)o["serverBaseUrl"])?.Trim().TrimEnd('/');
            if (!string.IsNullOrEmpty(url) && url != effective.serverBaseUrl)
            {
                changes.Add(new ConfigChange(ConfigChangeKind.Updated, label + "Server URL", $"{effective.serverBaseUrl} → {url}"));
                if (apply)
                    profile.serverBaseUrl = url == serverBaseUrl ? "" : url;
            }

            foreach (var incoming in incomingBuilds)
            {
                var current = effective.builds.Find(x => x != null && x.store == incoming.store)?.appId;
                if (current == incoming.appId)
                    continue;
                changes.Add(current == null
                    ? new ConfigChange(ConfigChangeKind.Added, label + $"Build {incoming.store}", incoming.appId)
                    : new ConfigChange(ConfigChangeKind.Updated, label + $"Build {incoming.store}", $"{current} → {incoming.appId}"));
                if (!apply)
                    continue;
                profile.builds = profile.builds ?? new List<BillingBuild>();
                profile.builds.RemoveAll(x => x?.store == incoming.store);
                if (builds?.Find(x => x != null && x.store == incoming.store)?.appId != incoming.appId)
                    profile.builds.Add(new BillingBuild(incoming.store, incoming.appId));
            }

            if (items == null)
                return changes;
            var catalog = new JArray();
            foreach (var item in items)
            {
                var parsed = ParseProducts(new JArray(item.DeepClone())).FirstOrDefault();
                var existing = parsed == null ? null
                    : FindById(parsed.id)
                      ?? products?.Find(x => x != null && parsed.AllIds().Any(x.Matches))
                      ?? effective.products.Where(x => x != null && parsed.AllIds().Any(x.Matches)).Select(x => FindById(x.id)).FirstOrDefault();
                if (existing == null)
                {
                    // A product the config does not have joins it with the ids the admin panel gave.
                    catalog.Add(item.DeepClone());
                    continue;
                }
                var effectiveProduct = effective.FindById(existing.id);
                foreach (var s in parsed.storeIds ?? new List<StoreProductId>())
                {
                    var now = effectiveProduct.StoreId(s.store);
                    if (now == s.id)
                        continue;
                    changes.Add(new ConfigChange(ConfigChangeKind.Updated, label + $"Product {existing.id}", $"{s.store} id {now} → {s.id}"));
                    if (!apply)
                        continue;
                    profile.productIds = profile.productIds ?? new List<ProductIdOverride>();
                    profile.productIds.RemoveAll(x => x?.product == existing.id && x.store == s.store);
                    if (existing.StoreId(s.store) != s.id)
                        profile.productIds.Add(new ProductIdOverride(existing.id, s.store, s.id));
                }
                // The catalog part: the config's own store ids stay as they are.
                var copy = (JObject)item.DeepClone();
                copy["id"] = existing.id;
                var own = (existing.storeIds ?? new List<StoreProductId>())
                    .Where(s => s != null && !string.IsNullOrEmpty(s.store) && !string.IsNullOrEmpty(s.id) && s.id != existing.id)
                    .GroupBy(s => s.store)
                    .ToList();
                if (own.Count == 0)
                    copy.Remove("storeIds");
                else
                    copy["storeIds"] = new JObject(own.Select(g => new JProperty(g.Key, g.First().id)));
                catalog.Add(copy);
            }
            changes.AddRange(Merge(new JObject { ["products"] = catalog }.ToString(), mode, apply));
            return changes;
        }

        private static JObject ParseJson(string json)
        {
            try
            {
                return JObject.Parse(json ?? "");
            }
            catch (Newtonsoft.Json.JsonException e)
            {
                throw new FormatException("not JSON: " + e.Message);
            }
        }

        private List<ConfigChange> Merge(string json, ConfigMergeMode mode, bool apply)
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

            // Parse everything first: a bad item must not leave a half-merged config.
            var incomingBuilds = o["builds"] is JArray b ? ParseBuilds(b) : new List<BillingBuild>();
            var incomingProducts = o["products"] is JArray p ? ParseProducts(p) : new List<BillingProductConfig>();
            // Mirror only the lists the JSON has: a config without "products" does not empty the products.
            var mirrorBuilds = mode == ConfigMergeMode.Mirror && o["builds"] is JArray;
            var mirrorProducts = mode == ConfigMergeMode.Mirror && o["products"] is JArray;
            var url = ((string)o["serverBaseUrl"])?.Trim().TrimEnd('/');
            var changes = new List<ConfigChange>();

            if (!string.IsNullOrEmpty(url) && url != serverBaseUrl)
            {
                changes.Add(new ConfigChange(ConfigChangeKind.Updated, "Server URL", $"{serverBaseUrl} → {url}"));
                if (apply)
                    serverBaseUrl = url;
            }

            if (apply && builds == null)
                builds = new List<BillingBuild>();
            foreach (var incoming in incomingBuilds)
            {
                var current = builds?.Find(x => x != null && x.store == incoming.store);
                if (current == null)
                {
                    changes.Add(new ConfigChange(ConfigChangeKind.Added, $"Build {incoming.store}", incoming.appId));
                    if (apply)
                        builds.Add(incoming);
                }
                else if (current.appId != incoming.appId)
                {
                    changes.Add(new ConfigChange(ConfigChangeKind.Updated, $"Build {incoming.store}", $"{current.appId} → {incoming.appId}"));
                    if (apply)
                        current.appId = incoming.appId;
                }
            }

            if (mirrorBuilds)
            {
                foreach (var stale in (builds ?? new List<BillingBuild>()).Where(x => x != null && incomingBuilds.All(i => i.store != x.store)).ToList())
                {
                    changes.Add(new ConfigChange(ConfigChangeKind.Removed, $"Build {stale.store}", stale.appId));
                    if (apply)
                        builds.Remove(stale);
                }
            }

            if (apply && products == null)
                products = new List<BillingProductConfig>();
            var matched = new HashSet<BillingProductConfig>();
            foreach (var incoming in incomingProducts)
            {
                var current = products?.Find(x => x != null && x.id == incoming.id)
                              ?? products?.Find(x => x != null && incoming.AllIds().Any(x.Matches));
                if (current == null)
                {
                    changes.Add(new ConfigChange(ConfigChangeKind.Added, $"Product {incoming.id}", Describe(incoming)));
                    if (apply)
                    {
                        incoming.access = incoming.access ?? new string[0];
                        products.Add(incoming);
                    }
                    matched.Add(incoming);
                    continue;
                }
                matched.Add(current);
                var staleIds = mirrorProducts
                    ? (current.storeIds ?? new List<StoreProductId>())
                        // A store id the incoming product is known by (it matched by it) is not stale.
                        .Where(s => s != null && !string.IsNullOrEmpty(s.id) && !incoming.AllIds().Contains(s.id) &&
                                    (incoming.storeIds ?? new List<StoreProductId>()).All(i => i.store != s.store))
                        .ToList()
                    : new List<StoreProductId>();
                var diffs = new List<string>();
                if (current.type != incoming.type)
                    diffs.Add($"type {current.type} → {incoming.type}");
                foreach (var s in incoming.storeIds ?? new List<StoreProductId>())
                {
                    var old = current.StoreId(s.store);
                    if (old != s.id)
                        diffs.Add($"{s.store} id {old} → {s.id}");
                }
                foreach (var s in staleIds)
                    diffs.Add($"{s.store} id {s.id} removed");
                var priceChanged = incoming.HasFallbackPrice && incoming.FallbackPriceText != current.FallbackPriceText;
                if (priceChanged)
                    diffs.Add($"price {(current.HasFallbackPrice ? current.FallbackPriceText : "none")} → {incoming.FallbackPriceText}");
                var periodChanged = incoming.HasFallbackPeriod && incoming.fallbackPeriod != BillingPeriods.Normalize(current.fallbackPeriod);
                if (periodChanged)
                    diffs.Add($"period {(current.HasFallbackPeriod ? current.fallbackPeriod : "none")} → {incoming.fallbackPeriod}");
                var accessChanged = incoming.access != null && !incoming.AccessLevels().SequenceEqual(current.AccessLevels());
                if (accessChanged)
                    diffs.Add($"access {AccessText(current)} → {AccessText(incoming)}");
                if (diffs.Count == 0)
                    continue;
                changes.Add(new ConfigChange(ConfigChangeKind.Updated, $"Product {current.id}", string.Join("; ", diffs)));
                if (!apply)
                    continue;
                current.type = incoming.type;
                foreach (var s in incoming.storeIds ?? new List<StoreProductId>())
                    current.WithStoreId(s.store, s.id);
                foreach (var s in staleIds)
                    current.storeIds.Remove(s);
                if (priceChanged)
                    current.WithFallbackPrice(incoming.fallbackPrice, incoming.fallbackCurrency.ToUpperInvariant());
                if (periodChanged)
                    current.WithFallbackPeriod(incoming.fallbackPeriod);
                if (accessChanged)
                    current.WithAccess(incoming.access);
            }
            if (mirrorProducts)
            {
                foreach (var stale in (products ?? new List<BillingProductConfig>()).Where(x => x != null && !matched.Contains(x)).ToList())
                {
                    changes.Add(new ConfigChange(ConfigChangeKind.Removed, $"Product {stale.id}", Describe(stale)));
                    if (apply)
                        products.Remove(stale);
                }
            }
            return changes;
        }

        private static string Describe(BillingProductConfig p)
        {
            var parts = new List<string> { p.type.ToString() };
            parts.AddRange((p.storeIds ?? new List<StoreProductId>()).Where(s => s != null).Select(s => $"{s.store} id {s.id}"));
            if (p.HasFallbackPrice)
                parts.Add(p.FallbackPriceText);
            if (p.HasFallbackPeriod)
                parts.Add("every " + p.fallbackPeriod);
            if (p.AccessLevels().Count > 0)
                parts.Add("access " + AccessText(p));
            return string.Join("; ", parts);
        }

        private static string AccessText(BillingProductConfig p)
        {
            var levels = p.AccessLevels();
            return levels.Count == 0 ? "none" : string.Join(", ", levels);
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
                // Not listed: null, so a merge keeps the product's access levels.
                product.access = null;
                if (item["access"] is JArray levels)
                {
                    if (levels.Any(l => l.Type != JTokenType.String))
                        throw new FormatException($"product '{id}': access must be a list of names");
                    product.WithAccess(levels.Select(l => (string)l).ToArray());
                }
                else if (item["access"] != null && item["access"].Type != JTokenType.Null)
                    throw new FormatException($"product '{id}': access must be a list of names");
                var price = item["price"];
                if (price != null && price.Type != JTokenType.Null)
                {
                    if (price.Type != JTokenType.Float && price.Type != JTokenType.Integer || (double)price < 0)
                        throw new FormatException($"product '{id}': price must be a number of at least 0");
                    product.WithFallbackPrice((double)price, ((string)item["currency"])?.Trim().ToUpperInvariant());
                }
                var period = (string)item["period"];
                if (!string.IsNullOrWhiteSpace(period))
                {
                    if (BillingPeriods.Normalize(period).Length == 0)
                        throw new FormatException($"product '{id}': period must be ISO 8601 like P1M, not '{period}'");
                    product.WithFallbackPeriod(period);
                }
                if (item["storeIds"] is JObject storeIds)
                    foreach (var kv in storeIds)
                        if (!string.IsNullOrEmpty((string)kv.Value))
                            product.WithStoreId(kv.Key, (string)kv.Value);
                parsed.Add(product);
            }
            return parsed;
        }

        /// <summary>The config as JSON for the admin panel ("Paste from Unity"); the inverse of <see cref="MergeJson"/>.</summary>
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
                        if (p.HasFallbackPrice)
                        {
                            item["price"] = Math.Round(p.fallbackPrice, 6);
                            item["currency"] = p.fallbackCurrency.ToUpperInvariant();
                        }
                        if (p.HasFallbackPeriod)
                            item["period"] = BillingPeriods.Normalize(p.fallbackPeriod);
                        if (p.AccessLevels().Count > 0)
                            item["access"] = new JArray(p.AccessLevels());
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
            // The config's own URL matters when a game runs without a profile or with one that keeps it.
            var chosen = new[] { editorProfile, developmentProfile, releaseProfile };
            var usesOwnUrl = chosen.Any(n => string.IsNullOrWhiteSpace(FindProfile(n?.Trim())?.serverBaseUrl));
            if (usesOwnUrl)
                AddUrlProblem(problems, "Server base URL", serverBaseUrl);

            if (requestTimeoutSeconds < 1)
                problems.Add("Request timeout must be at least 1 second.");
            if (entitlementsMaxAgeDays < 0)
                problems.Add("Entitlements max age must not be negative (0 = no limit).");
            if (!string.IsNullOrWhiteSpace(responseSigningPublicKey) && ResponseSignature.DecodeKey(responseSigningPublicKey) == null)
                problems.Add("Response signing public key must be a base64 Ed25519 public key (32 bytes); every signed answer is rejected until it is.");
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

            ValidateProfiles(problems);

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
                    else if (p.fallbackPrice > 0 && (p.fallbackCurrency ?? "").Trim().Length != 3)
                        problems.Add($"Product '{p.id}': the fallback price needs a three-letter currency (USD, EUR, ...).");
                    else if (!string.IsNullOrWhiteSpace(p.fallbackPeriod) && !p.HasFallbackPeriod)
                        problems.Add($"Product '{p.id}': the fallback period must be ISO 8601 like P1M.");
                }
            }
            return problems;
        }

        private static void AddUrlProblem(List<string> problems, string what, string url)
        {
            if (string.IsNullOrWhiteSpace(url))
                problems.Add($"{what} is empty.");
            else if (!UrlPolicy.IsAllowedServerUrl(url))
                problems.Add($"{what} must use https:// (plain http only for localhost); player builds refuse other URLs.");
            else if (url.EndsWith("/"))
                problems.Add($"{what} must not end with '/'.");
            else if (url.Contains(ServerUrlPlaceholder))
                problems.Add($"Set the {what.ToLowerInvariant()} to your billing database (replace {ServerUrlPlaceholder}).");
        }

        private void ValidateProfiles(List<string> problems)
        {
            var names = new HashSet<string>();
            foreach (var p in profiles ?? new List<BillingProfile>())
            {
                if (p == null || string.IsNullOrWhiteSpace(p.name))
                {
                    problems.Add("A profile has no name.");
                    continue;
                }
                if (!names.Add(p.name))
                {
                    problems.Add($"Two profiles named '{p.name}': keep one.");
                    continue;
                }
                if (!string.IsNullOrWhiteSpace(p.serverBaseUrl))
                    AddUrlProblem(problems, $"Profile '{p.name}': server base URL", p.serverBaseUrl.Trim());
                if (!string.IsNullOrWhiteSpace(p.responseSigningPublicKey) && ResponseSignature.DecodeKey(p.responseSigningPublicKey.Trim()) == null)
                    problems.Add($"Profile '{p.name}': the response signing public key must be a base64 Ed25519 public key (32 bytes).");
                var stores = new HashSet<string>();
                foreach (var b in p.builds ?? new List<BillingBuild>())
                {
                    if (b == null || string.IsNullOrWhiteSpace(b.store) || string.IsNullOrWhiteSpace(b.appId))
                        problems.Add($"Profile '{p.name}': a build needs a store and an app id.");
                    else if (!stores.Add(b.store))
                        problems.Add($"Profile '{p.name}': two builds for the store '{b.store}'.");
                    else if (!BillingStores.All.Contains(b.store))
                        problems.Add($"Profile '{p.name}': unknown store '{b.store}'.");
                }
                foreach (var o in p.productIds ?? new List<ProductIdOverride>())
                {
                    if (o == null || string.IsNullOrWhiteSpace(o.product) || string.IsNullOrWhiteSpace(o.store) || string.IsNullOrWhiteSpace(o.id))
                        problems.Add($"Profile '{p.name}': a product id needs the product, a store and the id.");
                    else if (FindById(o.product) == null)
                        problems.Add($"Profile '{p.name}': no product '{o.product}' for its {o.store} id.");
                    else if (!BillingStores.All.Contains(o.store))
                        problems.Add($"Profile '{p.name}': unknown store '{o.store}' for product '{o.product}'.");
                }
            }
            foreach (var (label, value) in new[] { ("Editor", editorProfile), ("Development", developmentProfile), ("Release", releaseProfile) })
            {
                if (!string.IsNullOrWhiteSpace(value) && FindProfile(value.Trim()) == null)
                    problems.Add($"{label} profile '{value.Trim()}' does not exist.");
            }
            if (FindProfile(releaseProfile?.Trim())?.isTest == true)
                problems.Add($"The release profile '{releaseProfile.Trim()}' is a test environment: release builds would use its server.");
        }
    }
}
