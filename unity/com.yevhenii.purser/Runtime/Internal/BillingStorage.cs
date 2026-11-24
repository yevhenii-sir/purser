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

    /// <summary>
    /// An <see cref="IBillingStorage"/> that does not write every <see cref="IBillingStorage.Save"/> to disk at once.
    /// <see cref="BillingService"/> flushes it when the app is paused or quits and when it is disposed; the package
    /// flushes right after a write that must not be lost (a started web order, the refund data consent).
    /// </summary>
    public interface IFlushableBillingStorage : IBillingStorage
    {
        /// <summary>Writes what was saved since the last flush to disk; cheap when nothing was.</summary>
        void Flush();
    }

    /// <summary>Default storage: <c>PlayerPrefs</c>. A value is in PlayerPrefs right after <see cref="Save"/> and reaches
    /// the disk at the next <see cref="Flush"/> (Unity also writes PlayerPrefs when the app quits).</summary>
    public sealed class PlayerPrefsBillingStorage : IFlushableBillingStorage
    {
        /// <summary>PlayerPrefs is one store for the whole app: one flag for every instance.</summary>
        private static bool s_Dirty;

        /// <inheritdoc/>
        public string Load(string key) => PlayerPrefs.GetString(key, null);

        /// <inheritdoc/>
        public void Save(string key, string value)
        {
            PlayerPrefs.SetString(key, value);
            s_Dirty = true;
        }

        /// <inheritdoc/>
        public void Flush()
        {
            if (!s_Dirty)
                return;
            s_Dirty = false;
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

    /// <summary>The exact bytes of a signed server answer with its signature (<see cref="ApiResult{T}.RawBody"/>).</summary>
    internal readonly struct SignedAnswer
    {
        public readonly byte[] Body;
        public readonly string Signature;
        /// <summary>The body is a <c>/restore</c> answer: the customer info is its <c>entitlements</c>.</summary>
        public readonly bool IsRestore;

        public SignedAnswer(byte[] body, string signature, bool isRestore)
        {
            Body = body;
            Signature = signature;
            IsRestore = isRestore;
        }

        public bool IsSigned => Body != null && !string.IsNullOrEmpty(Signature);
    }

    /// <summary>
    /// Which cache keys (customer info, paywalls) belong to the player whose data was cached last, per app: when
    /// another player's data is cached, the previous player's keys are emptied, so the device keeps one player's cache.
    /// </summary>
    internal static class CacheIndex
    {
        private const string KeyPrefix = "purser.cache_keys.";

        private sealed class Index
        {
            [JsonProperty("user")] public string User;
            [JsonProperty("keys")] public List<string> Keys = new List<string>();
        }

        /// <summary>Call before writing the cache <paramref name="key"/> of <paramref name="userId"/>.</summary>
        public static void Track(IBillingStorage storage, string appId, string userId, string key, BillingLog log)
        {
            if (string.IsNullOrEmpty(userId))
                return;
            try
            {
                var indexKey = KeyPrefix + appId;
                var json = storage.Load(indexKey);
                var index = (string.IsNullOrEmpty(json) ? null : JsonConvert.DeserializeObject<Index>(json)) ?? new Index();
                if (index.Keys == null)
                    index.Keys = new List<string>();
                var changed = false;
                if (index.User != userId)
                {
                    foreach (var old in index.Keys)
                        if (old != key && !string.IsNullOrEmpty(storage.Load(old)))
                            storage.Save(old, "");
                    index = new Index { User = userId };
                    changed = true;
                }
                if (!index.Keys.Contains(key))
                {
                    index.Keys.Add(key);
                    changed = true;
                }
                if (changed)
                    storage.Save(indexKey, JsonConvert.SerializeObject(index));
            }
            catch (Exception e)
            {
                log?.Warning("Could not update the list of cached keys: " + e.Message);
            }
        }
    }

    /// <summary>The last customer info per app and player, for offline starts.</summary>
    internal static class CustomerInfoCache
    {
        private const string KeyPrefix = "purser.customer_info.";

        private sealed class Snapshot
        {
            /// <summary>The customer info of an answer without a signature.</summary>
            [JsonProperty("dto", NullValueHandling = NullValueHandling.Ignore)] public CustomerInfoDto Dto;
            [JsonProperty("fetched_at_ticks")] public long FetchedAtTicks;
            /// <summary>A signed answer, base64 of its exact bytes: the customer info is read from it.</summary>
            [JsonProperty("body", NullValueHandling = NullValueHandling.Ignore)] public string Body;
            [JsonProperty("signature", NullValueHandling = NullValueHandling.Ignore)] public string Signature;
            /// <summary><see cref="Body"/> is a <c>/restore</c> answer.</summary>
            [JsonProperty("restore", DefaultValueHandling = DefaultValueHandling.Ignore)] public bool Restore;
        }

        /// <summary>Saves the customer info of <paramref name="userId"/>. A signed answer is kept as its bytes and
        /// signature, so <see cref="Load"/> can check it again.</summary>
        public static void Save(IBillingStorage storage, string appId, string userId, CustomerInfoDto dto, DateTime fetchedAtUtc,
            BillingLog log, SignedAnswer signed = default)
        {
            try
            {
                var snapshot = signed.IsSigned
                    ? new Snapshot { Body = Convert.ToBase64String(signed.Body), Signature = signed.Signature, Restore = signed.IsRestore }
                    : new Snapshot { Dto = dto };
                snapshot.FetchedAtTicks = fetchedAtUtc.Ticks;
                var key = Key(appId, userId);
                CacheIndex.Track(storage, appId, userId, key, log);
                storage.Save(key, JsonConvert.SerializeObject(snapshot));
            }
            catch (Exception e)
            {
                log?.Warning("Could not cache the customer info for offline starts: " + e.Message);
            }
        }

        /// <summary>
        /// The cached customer info of <paramref name="userId"/>, or null. With <paramref name="publicKey"/> (signed
        /// responses are on) only a copy whose signature is valid and that names this player counts: one that was
        /// edited, or saved without a signature, is ignored.
        /// </summary>
        public static CustomerInfo Load(IBillingStorage storage, string appId, string userId, BillingLog log, string publicKey = null)
        {
            try
            {
                var json = storage.Load(Key(appId, userId));
                if (string.IsNullOrEmpty(json))
                    return null;
                var s = JsonConvert.DeserializeObject<Snapshot>(json);
                if (s == null)
                    return null;
                var dto = s.Dto;
                if (!string.IsNullOrEmpty(s.Body))
                {
                    var body = Convert.FromBase64String(s.Body);
                    if (!string.IsNullOrEmpty(publicKey))
                    {
                        var problem = ResponseSignature.Problem(publicKey, body, s.Signature);
                        if (problem != null)
                        {
                            log?.Warning("The cached customer info is ignored: " + problem);
                            return null;
                        }
                    }
                    var text = System.Text.Encoding.UTF8.GetString(body);
                    dto = s.Restore
                        ? JsonConvert.DeserializeObject<RestoreResponse>(text)?.CustomerInfo
                        : JsonConvert.DeserializeObject<CustomerInfoDto>(text);
                    if (dto != null && !string.IsNullOrEmpty(publicKey) && dto.UserId != userId)
                    {
                        log?.Warning("The cached customer info is ignored: it is signed for another player.");
                        return null;
                    }
                }
                else if (!string.IsNullOrEmpty(publicKey))
                {
                    if (dto != null)
                        log?.Warning("The cached customer info is ignored: it has no signature.");
                    return null;
                }
                return dto == null ? null : CustomerInfo.FromDto(userId, dto, new DateTime(s.FetchedAtTicks, DateTimeKind.Utc), true);
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
