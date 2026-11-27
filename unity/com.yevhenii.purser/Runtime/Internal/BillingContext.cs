using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// What <see cref="BillingService"/> shares with its parts (<see cref="OrderProcessor"/>,
    /// <see cref="CustomerInfoRefresher"/>): the collaborators, the lifetime of the service and the current player.
    /// Used from the Unity main thread only, like the service.
    /// </summary>
    internal sealed class BillingContext
    {
        /// <summary>Storage key of the player's answer to <see cref="IBillingService.SetRefundDataConsent"/>.</summary>
        internal const string RefundConsentKey = "purser.refund_data_consent";

        public readonly BillingConfig Config;
        public readonly IStoreAdapter Store;
        public readonly IBillingApi Api;
        public readonly IBillingStorage Storage;
        public readonly BillingLog Log;
        /// <summary>The device clock (UTC).</summary>
        public readonly Func<DateTime> UtcNow;
        public readonly Func<TimeSpan, CancellationToken, Task> Delay;
        public readonly bool IsEditor;
        /// <summary>The device, read once on the main thread (Unity APIs).</summary>
        public readonly ClientInfoDto Client;
        /// <summary>Firebase <c>app_instance_id</c> of the game; may be null.</summary>
        public readonly IAnalyticsIdProvider Analytics;
        /// <summary>Device ids for attribution integrations; may be null.</summary>
        public readonly IAttributionProvider Attribution;

        /// <summary>How long a provider of the game (attribution ids, app instance id) may take to answer; a request
        /// that needs its value goes without it after that.</summary>
        internal static readonly TimeSpan ProviderTimeout = TimeSpan.FromSeconds(5);

        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        private bool m_AttributionTimedOut;
        private bool m_AnalyticsTimedOut;

        public BillingContext(BillingConfig config, IStoreAdapter store, IBillingApi api, IBillingStorage storage, BillingLog log,
            Func<DateTime> utcNow, Func<TimeSpan, CancellationToken, Task> delay, bool isEditor, ClientInfoDto client,
            IAnalyticsIdProvider analytics, IAttributionProvider attribution)
        {
            Config = config;
            Store = store;
            Api = api;
            Storage = storage;
            Log = log;
            UtcNow = utcNow;
            Delay = delay;
            IsEditor = isEditor;
            Client = client;
            Analytics = analytics;
            Attribution = attribution;
        }

        /// <summary>The user id last read from the user id provider (<c>BillingService.SyncUser</c>); null when there
        /// is no player.</summary>
        public string User { get; set; }

        /// <summary>The service was disposed: nothing is started or reported any more.</summary>
        public bool IsDisposed { get; private set; }

        /// <summary>Cancelled when the service is disposed. Read it only while <see cref="IsDisposed"/> is false: the
        /// source is disposed with the service.</summary>
        public CancellationToken Lifetime => m_Lifetime.Token;

        /// <summary>The store of the build (<see cref="IStoreAdapter.Store"/>), sent with <see cref="AppId"/>: the server
        /// finds the build by both.</summary>
        public string BuildStore => Store.Store;

        /// <summary>The build's app id in its store (<see cref="BillingConfig.AppIdFor"/>).</summary>
        public string AppId => Config.AppIdFor(BuildStore);

        /// <summary>Key of the player caches (customer info, paywalls): the app id on this server. Another server (a
        /// dev profile) keeps caches of its own.</summary>
        public string CacheScope => m_CacheScope ?? (m_CacheScope = CacheScopeOf(AppId, Config.serverBaseUrl));
        private string m_CacheScope;

        internal static string CacheScopeOf(string appId, string serverBaseUrl)
        {
            using (var sha = System.Security.Cryptography.SHA256.Create())
            {
                var hash = sha.ComputeHash(System.Text.Encoding.UTF8.GetBytes((serverBaseUrl ?? "").Trim().TrimEnd('/')));
                return appId + "@" + BitConverter.ToString(hash, 0, 4).Replace("-", "").ToLowerInvariant();
            }
        }

        /// <summary>Editor without the server: purchases are confirmed and granted locally.</summary>
        public bool SkipServer => IsEditor && Config.editorMode == EditorVerificationMode.SkipServer;

        /// <summary>Marks the service disposed and cancels <see cref="Lifetime"/>.</summary>
        public void Dispose()
        {
            IsDisposed = true;
            m_Lifetime.Cancel();
            m_Lifetime.Dispose();
        }

        /// <summary>The attribution ids of the game and the refund data consent, cleaned; null when there are none.
        /// A provider that does not answer within <see cref="ProviderTimeout"/> gives nothing.</summary>
        public async Task<Dictionary<string, string>> AttributionAsync()
        {
            var clean = new Dictionary<string, string>();
            if (Attribution != null)
            {
                try
                {
                    var (answered, values) = await TaskTimeout.Try(Attribution.GetAttributionAsync(), ProviderTimeout,
                        LifetimeOrCancelled, Delay);
                    if (!answered)
                        WarnOnce(ref m_AttributionTimedOut, "The attribution provider");
                    else if (values != null)
                        foreach (var kv in values.Where(kv => !string.IsNullOrEmpty(kv.Key) && !string.IsNullOrEmpty(kv.Value)))
                            // A child-directed app never sends advertising ids, whatever the provider gives.
                            if (!(Config.childDirected && AttributionKeys.Advertising.Contains(kv.Key)))
                                clean[kv.Key] = kv.Value;
                }
                catch (Exception e)
                {
                    Log.Verbose("Attribution ids unavailable: " + e.Message);
                }
            }
            // Always explicit ("false" too): the server keeps the last value it received.
            var consent = Storage.Load(RefundConsentKey);
            if (!string.IsNullOrEmpty(consent))
                clean[AttributionKeys.RefundDataConsent] = consent;
            return clean.Count == 0 ? null : clean;
        }

        /// <summary>Firebase <c>app_instance_id</c>; null when there is no provider, it failed or it did not answer
        /// within <see cref="ProviderTimeout"/>.</summary>
        public async Task<string> AppInstanceIdAsync()
        {
            if (Analytics == null)
                return null;
            try
            {
                var (answered, id) = await TaskTimeout.Try(Analytics.GetAppInstanceIdAsync(), ProviderTimeout,
                    LifetimeOrCancelled, Delay);
                if (!answered)
                    WarnOnce(ref m_AnalyticsTimedOut, "The analytics id provider");
                return string.IsNullOrEmpty(id) ? null : id;
            }
            catch (Exception e)
            {
                Log.Verbose("App instance id unavailable: " + e.Message);
                return null;
            }
        }

        /// <summary><see cref="Lifetime"/>, or a cancelled token once the service is disposed.</summary>
        private CancellationToken LifetimeOrCancelled => IsDisposed ? new CancellationToken(true) : m_Lifetime.Token;

        private void WarnOnce(ref bool warned, string provider)
        {
            if (warned)
                return;
            warned = true;
            Log.Warning($"{provider} did not answer in {ProviderTimeout.TotalSeconds:0} s: requests go without its value " +
                        "(reported once).");
        }
    }
}
