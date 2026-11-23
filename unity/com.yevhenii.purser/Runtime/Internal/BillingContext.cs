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

        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();

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

        /// <summary>The build's app id on the billing server.</summary>
        public string AppId => Config.AppIdFor(Store.Store);

        /// <summary>Editor without the server: purchases are confirmed and granted locally.</summary>
        public bool SkipServer => IsEditor && Config.editorMode == EditorVerificationMode.SkipServer;

        /// <summary>Marks the service disposed and cancels <see cref="Lifetime"/>.</summary>
        public void Dispose()
        {
            IsDisposed = true;
            m_Lifetime.Cancel();
            m_Lifetime.Dispose();
        }

        /// <summary>The attribution ids of the game and the refund data consent, cleaned; null when there are none.</summary>
        public async Task<Dictionary<string, string>> AttributionAsync()
        {
            var clean = new Dictionary<string, string>();
            if (Attribution != null)
            {
                try
                {
                    var values = await Attribution.GetAttributionAsync();
                    if (values != null)
                        foreach (var kv in values.Where(kv => !string.IsNullOrEmpty(kv.Key) && !string.IsNullOrEmpty(kv.Value)))
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

        /// <summary>Firebase <c>app_instance_id</c>; null when there is no provider or it failed.</summary>
        public async Task<string> AppInstanceIdAsync()
        {
            if (Analytics == null)
                return null;
            try
            {
                var id = await Analytics.GetAppInstanceIdAsync();
                return string.IsNullOrEmpty(id) ? null : id;
            }
            catch (Exception e)
            {
                Log.Verbose("App instance id unavailable: " + e.Message);
                return null;
            }
        }
    }
}
