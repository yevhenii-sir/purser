using System;
using System.Collections.Generic;

namespace Purser
{
    /// <summary>
    /// What the player owns according to the billing server: active subscriptions and non-consumables,
    /// across all platforms of the same game. Keys are the app's product ids (the admin panel's catalog id, which is the
    /// config's product <c>id</c>; a store product id for products without one). <see cref="IBillingService.IsActive"/>
    /// checks a configured product by its id and every store id.
    /// </summary>
    public sealed class Entitlements
    {
        /// <summary>A device clock this much behind <see cref="FetchedAtUtc"/> is a clock set back, not drift.</summary>
        private static readonly TimeSpan ClockSkewTolerance = TimeSpan.FromHours(1);

        /// <summary>Nothing owned (no user, or not loaded yet).</summary>
        public static readonly Entitlements Empty =
            new Entitlements(null, new Dictionary<string, long>(), new string[0], 0, DateTime.MinValue, true);

        /// <summary>Billing user these entitlements belong to.</summary>
        public string UserId { get; }

        /// <summary>Product id → expiration (Unix ms, 0 = never).</summary>
        public IReadOnlyDictionary<string, long> Subscriptions { get; }

        /// <summary>Owned non-consumable product ids.</summary>
        public IReadOnlyCollection<string> NonConsumables { get; }

        /// <summary>Server clock when the data was produced.</summary>
        public long ServerTimeMs { get; }

        /// <summary>When the device received the data from the server (device clock, UTC).</summary>
        public DateTime FetchedAtUtc { get; }

        /// <summary>True when the server could not be reached and this is the last saved copy.</summary>
        public bool IsFromCache { get; }

        private readonly HashSet<string> m_NonConsumables;

        /// <summary>Creates entitlements, e.g. for tests or a custom cache.</summary>
        public Entitlements(string userId, IDictionary<string, long> subscriptions, IEnumerable<string> nonConsumables,
            long serverTimeMs, DateTime fetchedAtUtc, bool isFromCache)
        {
            UserId = userId;
            Subscriptions = new Dictionary<string, long>(subscriptions ?? new Dictionary<string, long>());
            m_NonConsumables = new HashSet<string>(nonConsumables ?? new string[0]);
            NonConsumables = m_NonConsumables;
            ServerTimeMs = serverTimeMs;
            FetchedAtUtc = fetchedAtUtc;
            IsFromCache = isFromCache;
        }

        /// <summary>Server time now, estimated from the server clock and local elapsed time. A device clock set back
        /// before <see cref="FetchedAtUtc"/> counts as no time elapsed, never as time going backwards.</summary>
        public long EstimatedServerNowMs(DateTime utcNow)
        {
            if (ServerTimeMs <= 0)
                return new DateTimeOffset(utcNow).ToUnixTimeMilliseconds();
            var elapsedMs = (long)(utcNow - FetchedAtUtc).TotalMilliseconds;
            return ServerTimeMs + Math.Max(0, elapsedMs);
        }

        /// <summary>
        /// Whether the subscription data can still be relied on at <paramref name="utcNow"/>: it was received at most
        /// <paramref name="maxAge"/> ago by the device clock, and the clock was not set back before it was received.
        /// <see cref="IBillingService.IsActive"/> ignores subscriptions of stale data
        /// (<see cref="BillingConfig.entitlementsMaxAgeDays"/>) until the next refresh from the server.
        /// </summary>
        public bool IsFresh(DateTime utcNow, TimeSpan maxAge)
        {
            var age = utcNow - FetchedAtUtc;
            return age >= -ClockSkewTolerance && age <= maxAge;
        }

        /// <summary>Owns this non-consumable.</summary>
        public bool Owns(string storeProductId) => storeProductId != null && m_NonConsumables.Contains(storeProductId);

        /// <summary>Has this subscription and it has not expired by the estimated server time.</summary>
        public bool IsSubscriptionActive(string storeProductId, DateTime utcNow)
        {
            if (storeProductId == null || !Subscriptions.TryGetValue(storeProductId, out var expires))
                return false;
            return expires == 0 || expires > EstimatedServerNowMs(utcNow);
        }

        /// <summary>Owned non-consumable or active subscription.</summary>
        public bool IsActive(string storeProductId, DateTime utcNow) =>
            Owns(storeProductId) || IsSubscriptionActive(storeProductId, utcNow);

        /// <summary>Subscription expiration in UTC; null when not subscribed or never expires.</summary>
        public DateTime? GetExpiration(string storeProductId)
        {
            if (storeProductId == null || !Subscriptions.TryGetValue(storeProductId, out var expires) || expires == 0)
                return null;
            return DateTimeOffset.FromUnixTimeMilliseconds(expires).UtcDateTime;
        }

        internal Entitlements AsCached() =>
            new Entitlements(UserId, new Dictionary<string, long>(Subscriptions), m_NonConsumables, ServerTimeMs, FetchedAtUtc, true);

        /// <summary>Local grant used by the Editor fake store (no server).</summary>
        internal Entitlements WithLocalGrant(string userId, string storeProductId, BillingProductType type, DateTime utcNow)
        {
            var subs = new Dictionary<string, long>(Subscriptions);
            var owned = new HashSet<string>(m_NonConsumables);
            if (type == BillingProductType.Subscription)
                subs[storeProductId] = new DateTimeOffset(utcNow.AddDays(30)).ToUnixTimeMilliseconds();
            else if (type == BillingProductType.NonConsumable)
                owned.Add(storeProductId);
            return new Entitlements(userId, subs, owned, new DateTimeOffset(utcNow).ToUnixTimeMilliseconds(), utcNow, false);
        }
    }
}
