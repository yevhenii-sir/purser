using System;
using System.Collections.Generic;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Purser
{
    /// <summary>One product of a <see cref="Paywall"/>, in display order.</summary>
    public sealed class PaywallProduct
    {
        /// <summary>Id from <see cref="BillingConfig"/>: pass it to <see cref="IBillingService.PurchaseAsync(string, Paywall, System.Threading.CancellationToken)"/>.</summary>
        public string Id { get; }

        /// <summary>Product id in this platform's store.</summary>
        public string StoreId { get; }

        public BillingProductType Type { get; }

        public BillingProductConfig Config { get; }

        /// <summary>Store product with the localized price; null when the store did not return it
        /// (not initialized yet, unavailable in this country, ...).</summary>
        public StoreProduct StoreProduct { get; }

        public string LocalizedPrice => StoreProduct?.LocalizedPrice;

        public bool AvailableToPurchase => StoreProduct != null && StoreProduct.AvailableToPurchase;

        public PaywallProduct(BillingProductConfig config, string storeId, StoreProduct storeProduct)
        {
            Config = config ?? throw new ArgumentNullException(nameof(config));
            Id = config.id;
            StoreId = storeId;
            Type = config.type;
            StoreProduct = storeProduct;
        }

        public override string ToString() => $"{Id} {LocalizedPrice}".Trim();
    }

    /// <summary>
    /// What the server says to show at a placement: an ordered list of products and a free-form remote
    /// config, changed in the admin page without an app update. During an A/B test the player always
    /// gets the same variant. Buy with <see cref="IBillingService.PurchaseAsync(string, Paywall, System.Threading.CancellationToken)"/>
    /// so the purchase counts for the variant, and report the screen with <see cref="IBillingService.LogPaywallShownAsync"/>.
    /// </summary>
    public sealed class Paywall
    {
        private JToken m_RemoteConfig;
        private bool m_RemoteConfigParsed;

        public string PlacementId { get; }
        public string PaywallId { get; }
        public string Name { get; }

        /// <summary>Variant of the placement this player got (the default one outside tests).</summary>
        public string VariantId { get; }

        /// <summary>Running A/B test the player is in; empty outside tests.</summary>
        public string TestId { get; }

        /// <summary>Products configured both on the server and in <see cref="BillingConfig"/>, in the server's order.</summary>
        public IReadOnlyList<PaywallProduct> Products { get; }

        /// <summary>Server products that are not in <see cref="BillingConfig"/> (Unity IAP only knows configured
        /// products): add them to the config in the next app update.</summary>
        public IReadOnlyList<string> MissingProductIds { get; }

        /// <summary>Remote config as JSON text (<c>{}</c> when not set).</summary>
        public string RemoteConfigJson { get; }

        /// <summary>The answer came from the local cache because the server was unreachable.</summary>
        public bool IsFromCache { get; }

        /// <summary>When the answer was received from the server (UTC).</summary>
        public DateTime FetchedAtUtc { get; }

        public bool IsInTest => !string.IsNullOrEmpty(TestId);

        public Paywall(string placementId, string paywallId, string name, string variantId, string testId,
            IReadOnlyList<PaywallProduct> products, IReadOnlyList<string> missingProductIds, string remoteConfigJson,
            bool isFromCache, DateTime fetchedAtUtc)
        {
            PlacementId = placementId;
            PaywallId = paywallId;
            Name = name;
            VariantId = variantId ?? "";
            TestId = testId ?? "";
            Products = products ?? Array.Empty<PaywallProduct>();
            MissingProductIds = missingProductIds ?? Array.Empty<string>();
            RemoteConfigJson = string.IsNullOrEmpty(remoteConfigJson) ? "{}" : remoteConfigJson;
            IsFromCache = isFromCache;
            FetchedAtUtc = fetchedAtUtc;
        }

        /// <summary>Remote config parsed (object, array or value); null when it is not valid JSON.</summary>
        public JToken RemoteConfig
        {
            get
            {
                if (!m_RemoteConfigParsed)
                {
                    m_RemoteConfigParsed = true;
                    try
                    {
                        m_RemoteConfig = JToken.Parse(RemoteConfigJson);
                    }
                    catch (JsonException)
                    {
                        m_RemoteConfig = null;
                    }
                }
                return m_RemoteConfig;
            }
        }

        /// <summary>Remote config when it is a JSON object (the usual case), otherwise an empty object.</summary>
        public JObject RemoteConfigObject => RemoteConfig as JObject ?? new JObject();

        /// <summary>Remote config deserialized into your own type; <c>default</c> when it does not fit.</summary>
        public T GetRemoteConfig<T>()
        {
            try
            {
                return JsonConvert.DeserializeObject<T>(RemoteConfigJson);
            }
            catch (JsonException)
            {
                return default;
            }
        }

        /// <summary>One remote config value, e.g. <c>GetValue("title", "Go premium")</c>.</summary>
        public T GetValue<T>(string key, T fallback = default)
        {
            var token = RemoteConfigObject[key];
            if (token == null || token.Type == JTokenType.Null)
                return fallback;
            try
            {
                return token.ToObject<T>();
            }
            catch (Exception)
            {
                // The value does not convert to T: the caller's fallback is the documented answer.
                return fallback;
            }
        }

        public override string ToString() =>
            $"{PlacementId}: {PaywallId} variant {VariantId}{(IsInTest ? " test " + TestId : "")}{(IsFromCache ? " (cached)" : "")}";
    }

    /// <summary>Last server answer per app, player and placement, for offline starts.</summary>
    internal static class PaywallCache
    {
        private const string KeyPrefix = "purser.paywall.";

        private sealed class Snapshot
        {
            [JsonProperty("response")] public PaywallResponse Response;
            [JsonProperty("fetched_at_ticks")] public long FetchedAtTicks;
        }

        public static void Save(IBillingStorage storage, string appId, string userId, string placement, PaywallResponse response,
            DateTime fetchedAtUtc, BillingLog log)
        {
            try
            {
                var json = response == null ? "" : JsonConvert.SerializeObject(new Snapshot { Response = response, FetchedAtTicks = fetchedAtUtc.Ticks });
                storage.Save(Key(appId, userId, placement), json);
            }
            catch (Exception e)
            {
                log?.Warning($"Could not cache the paywall '{placement}' for offline starts: {e.Message}");
            }
        }

        public static (PaywallResponse Response, DateTime FetchedAtUtc)? Load(IBillingStorage storage, string appId, string userId,
            string placement, BillingLog log)
        {
            try
            {
                var json = storage.Load(Key(appId, userId, placement));
                if (string.IsNullOrEmpty(json))
                    return null;
                var s = JsonConvert.DeserializeObject<Snapshot>(json);
                return s?.Response == null ? ((PaywallResponse, DateTime)?)null : (s.Response, new DateTime(s.FetchedAtTicks, DateTimeKind.Utc));
            }
            catch (Exception e)
            {
                log?.Warning($"Could not read the cached paywall '{placement}': {e.Message}");
                return null;
            }
        }

        private static string Key(string appId, string userId, string placement) =>
            KeyPrefix + appId + "." + (string.IsNullOrEmpty(userId) ? "-" : userId) + "." + placement;
    }
}
