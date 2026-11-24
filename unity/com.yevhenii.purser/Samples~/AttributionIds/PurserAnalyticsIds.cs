// The ids Purser's server needs to send renewals, refunds and Xsolla purchases to the game's analytics:
// Tenjin's analytics_installation_id (Tenjin integration) and Firebase's app_instance_id (GA4 integration).
// 1. Add PURSER_TENJIN and/or PURSER_FIREBASE to Player Settings > Scripting Define Symbols when the game has the
//    Tenjin Unity SDK / Firebase Analytics.
// 2. Pass one instance as both providers:
//      var ids = new PurserAnalyticsIds("<tenjin sdk key>");
//      new BillingDependencies { AttributionProvider = ids, AnalyticsIdProvider = ids, ... }
// Child-directed apps: set BillingConfig.childDirected; advertising ids are then never sent.
using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser.Samples
{
    public sealed class PurserAnalyticsIds : IAttributionProvider, IAnalyticsIdProvider
    {
        private readonly string m_TenjinKey;

        public PurserAnalyticsIds(string tenjinSdkKey = null)
        {
            m_TenjinKey = tenjinSdkKey;
        }

        public Task<IDictionary<string, string>> GetAttributionAsync()
        {
            IDictionary<string, string> ids = new Dictionary<string, string>
            {
                // Tenjin's server API needs the OS and app versions next to its id.
                [AttributionKeys.OsVersion] = OsVersion(),
                [AttributionKeys.AppVersion] = Application.version,
            };
#if PURSER_TENJIN
            if (!string.IsNullOrEmpty(m_TenjinKey))
            {
                try
                {
                    var id = Tenjin.getInstance(m_TenjinKey).GetAnalyticsInstallationId();
                    if (!string.IsNullOrEmpty(id))
                        ids[AttributionKeys.TenjinId] = id;
                }
                catch (Exception e)
                {
                    Debug.LogWarning("[Purser] Tenjin id unavailable: " + e.Message);
                }
            }
#endif
            return Task.FromResult(ids);
        }

        public async Task<string> GetAppInstanceIdAsync()
        {
#if PURSER_FIREBASE
            try
            {
                return await Firebase.Analytics.FirebaseAnalytics.GetAnalyticsInstanceIdAsync();
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Purser] Firebase app_instance_id unavailable: " + e.Message);
            }
#endif
            await Task.CompletedTask;
            return null;
        }

        private static string OsVersion()
        {
#if UNITY_ANDROID && !UNITY_EDITOR
            using (var version = new AndroidJavaClass("android.os.Build$VERSION"))
                return version.GetStatic<string>("RELEASE");
#else
            return SystemInfo.operatingSystem;
#endif
        }
    }
}
