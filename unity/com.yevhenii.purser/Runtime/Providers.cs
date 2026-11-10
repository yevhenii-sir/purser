using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Supplies the id of the current player, exactly as the game backend knows it
    /// (for SpacetimeDB games: the player's Identity hex). Return null while the player is not known yet:
    /// purchases restored by the store at startup wait until an id is available.
    /// </summary>
    public interface IUserIdProvider
    {
        string GetUserId();
    }

    /// <summary>User id set from code (<see cref="IBillingService.SetUserId"/>).</summary>
    public sealed class StaticUserIdProvider : IUserIdProvider
    {
        public string UserId { get; set; }

        public StaticUserIdProvider(string userId = null)
        {
            UserId = string.IsNullOrEmpty(userId) ? null : userId;
        }

        public string GetUserId() => UserId;
    }

    /// <summary>User id read from a delegate, e.g. <c>() => conn.Identity?.ToString()</c>.</summary>
    public sealed class FuncUserIdProvider : IUserIdProvider
    {
        private readonly Func<string> m_Get;

        public FuncUserIdProvider(Func<string> get)
        {
            m_Get = get ?? throw new ArgumentNullException(nameof(get));
        }

        public string GetUserId()
        {
            var id = m_Get();
            return string.IsNullOrEmpty(id) ? null : id;
        }
    }

    /// <summary>
    /// Anonymous player id for games without accounts: a GUID created on first use and kept in PlayerPrefs.
    /// A reinstall gets a new id; non-consumables and subscriptions come back via <see cref="IBillingService.RestoreAsync"/>.
    /// Call from the main thread (PlayerPrefs).
    /// </summary>
    public sealed class LocalUserIdProvider : IUserIdProvider
    {
        public const string DefaultKey = "purser.user_id";

        private readonly string m_Key;
        private string m_UserId;

        public LocalUserIdProvider(string key = DefaultKey)
        {
            m_Key = string.IsNullOrEmpty(key) ? DefaultKey : key;
        }

        public string GetUserId()
        {
            if (m_UserId != null)
                return m_UserId;
            m_UserId = PlayerPrefs.GetString(m_Key, null);
            if (string.IsNullOrEmpty(m_UserId))
            {
                m_UserId = Guid.NewGuid().ToString("N");
                PlayerPrefs.SetString(m_Key, m_UserId);
                PlayerPrefs.Save();
            }
            return m_UserId;
        }
    }

    /// <summary>
    /// Optional Firebase <c>app_instance_id</c> (32 hex chars) so the server's GA4 events are attributed
    /// to the right device, e.g. <c>FirebaseAnalytics.GetAnalyticsInstanceIdAsync()</c>.
    /// </summary>
    public interface IAnalyticsIdProvider
    {
        Task<string> GetAppInstanceIdAsync();
    }

    /// <summary>Keys understood by the server's attribution integrations (AppsFlyer, Tenjin, ...).</summary>
    public static class AttributionKeys
    {
        /// <summary><c>AppsFlyer.getAppsFlyerId()</c> — required for AppsFlyer events.</summary>
        public const string AppsFlyerId = "appsflyer_id";
        /// <summary>Tenjin <c>analytics_installation_id</c> — required for Tenjin events.</summary>
        public const string TenjinId = "tenjin_id";
        /// <summary>Android GAID.</summary>
        public const string AdvertisingId = "advertising_id";
        public const string Oaid = "oaid";
        public const string Idfa = "idfa";
        public const string Idfv = "idfv";
        /// <summary>iOS ATT consent: "true" / "false".</summary>
        public const string Att = "att";
        /// <summary>Device OS version, e.g. "17.4" or "14" — required by Tenjin.</summary>
        public const string OsVersion = "os_version";
        /// <summary><c>Application.version</c>.</summary>
        public const string AppVersion = "app_version";
        /// <summary><c>Adjust.GetAdid()</c> — for Adjust events (or send a device id).</summary>
        public const string AdjustId = "adjust_id";
        /// <summary>AppMetrica device id hash (<c>AppMetrica.RequestAppMetricaDeviceID</c>) — for AppMetrica events.</summary>
        public const string AppMetricaDeviceId = "appmetrica_device_id";
        /// <summary>Apple Ads attribution, filled by <see cref="AppleAdsAttributionProvider"/>.</summary>
        public const string AppleAdsOrgId = "asa_org_id";
        public const string AppleAdsCampaignId = "asa_campaign_id";
        public const string AppleAdsAdGroupId = "asa_ad_group_id";
        public const string AppleAdsKeywordId = "asa_keyword_id";
        public const string AppleAdsAdId = "asa_ad_id";
        public const string AppleAdsCountry = "asa_country";
        /// <summary>
        /// The player consented to share consumption data with Apple when they ask for a refund:
        /// "true" / "false". Set it with <see cref="IBillingService.SetRefundDataConsent"/>.
        /// </summary>
        public const string RefundDataConsent = "refund_data_consent";
    }

    /// <summary>
    /// Optional device identifiers for attribution integrations (see <see cref="AttributionKeys"/>).
    /// They are sent with purchase verification; the server remembers them for renewals and refunds.
    /// Only provide identifiers the player consented to share.
    /// </summary>
    public interface IAttributionProvider
    {
        Task<IDictionary<string, string>> GetAttributionAsync();
    }

    /// <summary>Attribution from a delegate, e.g. <c>() => new Dictionary&lt;string, string&gt; { [AttributionKeys.AppsFlyerId] = AppsFlyer.getAppsFlyerId() }</c>.</summary>
    public sealed class FuncAttributionProvider : IAttributionProvider
    {
        private readonly Func<IDictionary<string, string>> m_Get;

        public FuncAttributionProvider(Func<IDictionary<string, string>> get)
        {
            m_Get = get ?? throw new ArgumentNullException(nameof(get));
        }

        public Task<IDictionary<string, string>> GetAttributionAsync() => Task.FromResult(m_Get());
    }
}
