using System;
using System.Collections.Generic;
using Newtonsoft.Json;
using UnityEngine;

namespace Purser
{
    /// <summary>Key-value persistence of the package: customer info and paywall caches (offline starts), started
    /// web and Steam orders, fulfilled Amazon receipts, the refund data consent.</summary>
    public interface IBillingStorage
    {
        /// <summary>The value saved under <paramref name="key"/>, or null (or empty) when there is none.</summary>
        string Load(string key);

        /// <summary>Saves <paramref name="value"/> under <paramref name="key"/>, replacing the old value.</summary>
        void Save(string key, string value);
    }

    /// <summary>Default storage: <c>PlayerPrefs</c>, saved to disk on every write.</summary>
    public sealed class PlayerPrefsBillingStorage : IBillingStorage
    {
        /// <inheritdoc/>
        public string Load(string key) => PlayerPrefs.GetString(key, null);

        /// <inheritdoc/>
        public void Save(string key, string value)
        {
            PlayerPrefs.SetString(key, value);
            PlayerPrefs.Save();
        }
    }

    /// <summary>Storage that keeps nothing across launches (tests, or games that must not write to disk).</summary>
    public sealed class MemoryBillingStorage : IBillingStorage
    {
        private readonly Dictionary<string, string> m_Values = new Dictionary<string, string>();

        /// <inheritdoc/>
        public string Load(string key) => m_Values.TryGetValue(key, out var v) ? v : null;

        /// <inheritdoc/>
        public void Save(string key, string value) => m_Values[key] = value;
    }

    /// <summary>The last customer info per app and player, for offline starts.</summary>
    internal static class CustomerInfoCache
    {
        private const string KeyPrefix = "purser.customer_info.";

        private sealed class Snapshot
        {
            [JsonProperty("dto")] public CustomerInfoDto Dto;
            [JsonProperty("fetched_at_ticks")] public long FetchedAtTicks;
        }

        public static void Save(IBillingStorage storage, string appId, string userId, CustomerInfoDto dto, DateTime fetchedAtUtc,
            BillingLog log)
        {
            try
            {
                var json = JsonConvert.SerializeObject(new Snapshot { Dto = dto, FetchedAtTicks = fetchedAtUtc.Ticks });
                storage.Save(Key(appId, userId), json);
            }
            catch (Exception e)
            {
                log?.Warning("Could not cache the customer info for offline starts: " + e.Message);
            }
        }

        public static CustomerInfo Load(IBillingStorage storage, string appId, string userId, BillingLog log)
        {
            try
            {
                var json = storage.Load(Key(appId, userId));
                if (string.IsNullOrEmpty(json))
                    return null;
                var s = JsonConvert.DeserializeObject<Snapshot>(json);
                return s?.Dto == null ? null : CustomerInfo.FromDto(userId, s.Dto, new DateTime(s.FetchedAtTicks, DateTimeKind.Utc), true);
            }
            catch (Exception e)
            {
                log?.Warning("Could not read the cached customer info: " + e.Message);
                return null;
            }
        }

        private static string Key(string appId, string userId) => KeyPrefix + appId + "." + userId;
    }
}
