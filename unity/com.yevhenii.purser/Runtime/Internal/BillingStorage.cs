using System;
using System.Collections.Generic;
using Newtonsoft.Json;
using UnityEngine;

namespace Purser
{
    /// <summary>Key-value persistence for the entitlements cache (offline start).</summary>
    public interface IBillingStorage
    {
        string Load(string key);
        void Save(string key, string value);
    }

    public sealed class PlayerPrefsBillingStorage : IBillingStorage
    {
        public string Load(string key) => PlayerPrefs.GetString(key, null);

        public void Save(string key, string value)
        {
            PlayerPrefs.SetString(key, value);
            PlayerPrefs.Save();
        }
    }

    public sealed class MemoryBillingStorage : IBillingStorage
    {
        private readonly Dictionary<string, string> m_Values = new Dictionary<string, string>();

        public string Load(string key) => m_Values.TryGetValue(key, out var v) ? v : null;

        public void Save(string key, string value) => m_Values[key] = value;
    }

    internal static class EntitlementsCache
    {
        private const string KeyPrefix = "purser.entitlements.";

        private sealed class Snapshot
        {
            [JsonProperty("dto")] public EntitlementsDto Dto;
            [JsonProperty("fetched_at_ticks")] public long FetchedAtTicks;
        }

        public static void Save(IBillingStorage storage, string appId, string userId, EntitlementsDto dto, DateTime fetchedAtUtc)
        {
            try
            {
                var json = JsonConvert.SerializeObject(new Snapshot { Dto = dto, FetchedAtTicks = fetchedAtUtc.Ticks });
                storage.Save(Key(appId, userId), json);
            }
            catch (Exception)
            {
                // Cache is best effort.
            }
        }

        public static Entitlements Load(IBillingStorage storage, string appId, string userId)
        {
            try
            {
                var json = storage.Load(Key(appId, userId));
                if (string.IsNullOrEmpty(json))
                    return null;
                var s = JsonConvert.DeserializeObject<Snapshot>(json);
                return s?.Dto == null ? null : FromDto(userId, s.Dto, new DateTime(s.FetchedAtTicks, DateTimeKind.Utc), true);
            }
            catch (Exception)
            {
                return null;
            }
        }

        public static Entitlements FromDto(string userId, EntitlementsDto dto, DateTime fetchedAtUtc, bool fromCache) =>
            new Entitlements(userId, dto.Subscriptions, dto.NonConsumables, dto.ServerTimeMs, fetchedAtUtc, fromCache);

        private static string Key(string appId, string userId) => KeyPrefix + appId + "." + userId;
    }
}
