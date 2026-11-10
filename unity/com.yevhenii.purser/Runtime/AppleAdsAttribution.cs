using System;
using System.Collections.Generic;
#if UNITY_IOS && !UNITY_EDITOR
using System.Runtime.InteropServices;
#endif
using System.Text;
using System.Threading.Tasks;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;
using UnityEngine.Networking;

namespace Purser
{
    /// <summary>
    /// Apple Ads attribution of the install (<see cref="BillingConfig.collectAppleAdsAttribution"/>).
    /// On iOS 14.3+ it takes an AdServices attribution token on the device, asks Apple's attribution API
    /// which campaign brought the install and adds the ids (<see cref="AttributionKeys.AppleAdsCampaignId"/>, ...)
    /// to the attribution sent with purchases. The answer is stored, so Apple is asked once per install
    /// (a few times at most while Apple has no record yet). Elsewhere it adds nothing.
    /// </summary>
    public sealed class AppleAdsAttributionProvider : IAttributionProvider
    {
        public const string ApiUrl = "https://api-adservices.apple.com/api/v1/";
        private const string StorageKey = "purser.apple_ads";
        /// <summary>Launches that try again while Apple answers "no record yet" or is unreachable.</summary>
        private const int MaxLaunches = 5;
        private static readonly Dictionary<string, string> Empty = new Dictionary<string, string>();

        private readonly IAttributionProvider m_Inner;
        private readonly IBillingStorage m_Storage;
        private readonly Func<string> m_TokenSource;
        private readonly Func<string, Task<(long Status, string Body)>> m_Post;
        private readonly Func<TimeSpan, Task> m_Delay;
        private Task<Dictionary<string, string>> m_Resolve;

        private sealed class State
        {
            [JsonProperty("done")] public bool Done;
            [JsonProperty("launches")] public int Launches;
            [JsonProperty("values")] public Dictionary<string, string> Values = new Dictionary<string, string>();
        }

        /// <param name="inner">The game's own attribution provider; its values are kept.</param>
        /// <param name="tokenSource">AdServices token (tests); defaults to the native plugin.</param>
        /// <param name="post">POST of the token to <see cref="ApiUrl"/> (tests).</param>
        public AppleAdsAttributionProvider(IAttributionProvider inner = null, IBillingStorage storage = null,
            Func<string> tokenSource = null, Func<string, Task<(long Status, string Body)>> post = null,
            Func<TimeSpan, Task> delay = null)
        {
            m_Inner = inner;
            m_Storage = storage ?? new PlayerPrefsBillingStorage();
            m_TokenSource = tokenSource ?? NativeToken;
            m_Post = post ?? PostAsync;
            m_Delay = delay ?? (d => Task.Delay(d));
        }

        /// <summary>Starts resolving right away (on launch), so the first purchase does not wait for Apple.</summary>
        public Task WarmUpAsync() => Resolve();

        public async Task<IDictionary<string, string>> GetAttributionAsync()
        {
            var result = new Dictionary<string, string>();
            if (m_Inner != null)
            {
                var own = await m_Inner.GetAttributionAsync();
                if (own != null)
                    foreach (var kv in own)
                        result[kv.Key] = kv.Value;
            }
            try
            {
                foreach (var kv in await Resolve())
                    result[kv.Key] = kv.Value;
            }
            catch (Exception)
            {
                // Attribution is best effort.
            }
            return result;
        }

        private Task<Dictionary<string, string>> Resolve()
        {
            if (m_Resolve == null || m_Resolve.IsFaulted || m_Resolve.IsCanceled)
                m_Resolve = ResolveCoreAsync();
            return m_Resolve;
        }

        private async Task<Dictionary<string, string>> ResolveCoreAsync()
        {
            var state = Load();
            if (state.Done)
                return state.Values ?? Empty;
            if (state.Launches >= MaxLaunches)
                return Empty;
            var token = m_TokenSource();
            if (string.IsNullOrEmpty(token))
                return Empty; // not iOS 14.3+, or the editor

            state.Launches++;
            Save(state);
            // Apple may need a moment to have the record: retry a few times, 5 s apart.
            for (var attempt = 0; attempt < 3; attempt++)
            {
                if (attempt > 0)
                    await m_Delay(TimeSpan.FromSeconds(5));
                var (status, body) = await m_Post(token);
                if (status == 200)
                {
                    state.Done = true;
                    state.Values = Parse(body);
                    Save(state);
                    return state.Values;
                }
                if (status == 400)
                    break; // invalid token: try again on the next launch with a new one
            }
            return Empty;
        }

        /// <summary>Attribution record → attribution keys; nothing when the install did not come from Apple Ads.</summary>
        public static Dictionary<string, string> Parse(string body)
        {
            var values = new Dictionary<string, string>();
            if (string.IsNullOrEmpty(body))
                return values;
            var o = JObject.Parse(body);
            if (o.Value<bool?>("attribution") != true)
                return values;
            void Add(string key, string field)
            {
                var t = o[field];
                if (t != null && t.Type != JTokenType.Null && t.ToString().Length > 0)
                    values[key] = t.ToString();
            }
            Add(AttributionKeys.AppleAdsOrgId, "orgId");
            Add(AttributionKeys.AppleAdsCampaignId, "campaignId");
            Add(AttributionKeys.AppleAdsAdGroupId, "adGroupId");
            Add(AttributionKeys.AppleAdsKeywordId, "keywordId");
            Add(AttributionKeys.AppleAdsAdId, "adId");
            Add(AttributionKeys.AppleAdsCountry, "countryOrRegion");
            return values;
        }

        private State Load()
        {
            try
            {
                var json = m_Storage.Load(StorageKey);
                return string.IsNullOrEmpty(json) ? new State() : JsonConvert.DeserializeObject<State>(json) ?? new State();
            }
            catch (Exception)
            {
                return new State();
            }
        }

        private void Save(State state)
        {
            try
            {
                m_Storage.Save(StorageKey, JsonConvert.SerializeObject(state));
            }
            catch (Exception)
            {
                // Best effort: Apple is asked again on the next launch.
            }
        }

#if UNITY_IOS && !UNITY_EDITOR
        [DllImport("__Internal")]
        private static extern string PurserAdServicesToken();

        private static string NativeToken()
        {
            try
            {
                return PurserAdServicesToken();
            }
            catch (Exception)
            {
                return null;
            }
        }
#else
        private static string NativeToken() => null;
#endif

        private static async Task<(long Status, string Body)> PostAsync(string token)
        {
            using (var request = new UnityWebRequest(ApiUrl, "POST"))
            {
                request.uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(token));
                request.SetRequestHeader("Content-Type", "text/plain");
                request.downloadHandler = new DownloadHandlerBuffer();
                request.timeout = 15;
                var tcs = new TaskCompletionSource<bool>();
                var operation = request.SendWebRequest();
                operation.completed += _ => tcs.TrySetResult(true);
                await tcs.Task;
                if (request.result == UnityWebRequest.Result.ConnectionError)
                    return (0, null);
                return (request.responseCode, request.downloadHandler?.text);
            }
        }
    }
}
