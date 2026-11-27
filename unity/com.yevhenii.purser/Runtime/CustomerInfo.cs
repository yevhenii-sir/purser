using System;
using System.Collections.Generic;
using System.Linq;

namespace Purser
{
    /// <summary>
    /// The state of one item the player has access through: an access level (<see cref="AccessInfo"/>) or a
    /// subscription (<see cref="SubscriptionInfo"/>), as the server reported it.
    /// </summary>
    public abstract class EntitlementInfo
    {
        /// <summary>Catalog product id (the config's product <c>id</c>) that gives it.</summary>
        public string ProductId { get; }

        /// <summary>Store of the purchase: <c>google</c>, <c>apple</c>, <c>amazon</c>, <c>steam</c>, <c>stripe</c>,
        /// <c>xsolla</c>, or <c>manual</c> (granted on the admin page).</summary>
        public string Store { get; }

        /// <summary>When access ends (UTC), grace period included; null = never (non-consumables, lifetime grants).</summary>
        public DateTime? ExpiresAt { get; }

        /// <summary>When it was bought (UTC); null when the server does not know.</summary>
        public DateTime? PurchasedAt { get; }

        /// <summary>The subscription renews at <see cref="ExpiresAt"/> (auto-renewal is on).</summary>
        public bool WillRenew { get; }

        /// <summary>The renewal payment failed and the store keeps access during its grace period.</summary>
        public bool IsInGracePeriod { get; }

        /// <summary>The store reports a billing problem (grace period or account hold): ask the player to update the
        /// payment method.</summary>
        public bool HasBillingIssue { get; }

        /// <summary>The current period is a free trial.</summary>
        public bool IsTrial { get; }

        /// <summary>Catalog id the subscription changes to at the next renewal (a pending downgrade); null when none.</summary>
        public string RenewsTo { get; }

        /// <summary>A family member shares it with the player (Apple Family Sharing); the player did not buy it.</summary>
        public bool IsFamilyShared { get; }

        /// <summary><see cref="ExpiresAt"/> as Unix milliseconds, 0 = never.</summary>
        internal long ExpiresAtMs { get; }

        internal long PurchasedAtMs { get; }

        /// <summary>Creates the state (tests, custom caches). Times are Unix milliseconds, 0 = never / unknown.</summary>
        protected EntitlementInfo(string productId, string store, long expiresAtMs, long purchasedAtMs, bool willRenew,
            bool isInGracePeriod, bool hasBillingIssue, bool isTrial, string renewsTo, bool isFamilyShared = false)
        {
            IsFamilyShared = isFamilyShared;
            ProductId = productId ?? "";
            Store = store ?? "";
            ExpiresAtMs = Math.Max(0, expiresAtMs);
            PurchasedAtMs = Math.Max(0, purchasedAtMs);
            ExpiresAt = ExpiresAtMs == 0 ? (DateTime?)null : DateTimeOffset.FromUnixTimeMilliseconds(ExpiresAtMs).UtcDateTime;
            PurchasedAt = PurchasedAtMs == 0 ? (DateTime?)null : DateTimeOffset.FromUnixTimeMilliseconds(PurchasedAtMs).UtcDateTime;
            WillRenew = willRenew;
            IsInGracePeriod = isInGracePeriod;
            HasBillingIssue = hasBillingIssue;
            IsTrial = isTrial;
            RenewsTo = string.IsNullOrEmpty(renewsTo) ? null : renewsTo;
        }

        /// <summary>Never expires.</summary>
        internal bool IsLifetime => ExpiresAtMs == 0;

        internal bool SameAs(EntitlementInfo other) =>
            other != null && ProductId == other.ProductId && Store == other.Store && ExpiresAtMs == other.ExpiresAtMs &&
            PurchasedAtMs == other.PurchasedAtMs && WillRenew == other.WillRenew && IsInGracePeriod == other.IsInGracePeriod &&
            HasBillingIssue == other.HasBillingIssue && IsTrial == other.IsTrial && RenewsTo == other.RenewsTo &&
            IsFamilyShared == other.IsFamilyShared;

        internal EntitlementStateDto ToDto() => new EntitlementStateDto
        {
            ProductId = ProductId,
            Store = Store,
            ExpiresAtMs = ExpiresAtMs,
            PurchasedAtMs = PurchasedAtMs,
            WillRenew = WillRenew,
            InGrace = IsInGracePeriod,
            BillingIssue = HasBillingIssue,
            Trial = IsTrial,
            RenewsTo = RenewsTo,
            FamilyShared = IsFamilyShared,
        };

        /// <inheritdoc/>
        public override string ToString() => $"{ProductId} ({Store}) until {(ExpiresAt?.ToString("u") ?? "forever")}";
    }

    /// <summary>An access level the player has (e.g. <c>premium</c>, <c>no_ads</c>): a name products unlock, set per
    /// product on the server. Check it with <see cref="IBillingService.HasAccess"/>.</summary>
    public sealed class AccessInfo : EntitlementInfo
    {
        /// <inheritdoc/>
        public AccessInfo(string productId, string store, long expiresAtMs, long purchasedAtMs = 0, bool willRenew = false,
            bool isInGracePeriod = false, bool hasBillingIssue = false, bool isTrial = false, string renewsTo = null,
            bool isFamilyShared = false)
            : base(productId, store, expiresAtMs, purchasedAtMs, willRenew, isInGracePeriod, hasBillingIssue, isTrial, renewsTo,
                isFamilyShared)
        {
        }

        internal static AccessInfo From(EntitlementStateDto d) => d == null
            ? null
            : new AccessInfo(d.ProductId, d.Store, d.ExpiresAtMs, d.PurchasedAtMs, d.WillRenew, d.InGrace, d.BillingIssue, d.Trial, d.RenewsTo,
                d.FamilyShared);
    }

    /// <summary>A subscription that gives access now, keyed by its catalog product id.</summary>
    public sealed class SubscriptionInfo : EntitlementInfo
    {
        /// <inheritdoc/>
        public SubscriptionInfo(string productId, string store, long expiresAtMs, long purchasedAtMs = 0, bool willRenew = false,
            bool isInGracePeriod = false, bool hasBillingIssue = false, bool isTrial = false, string renewsTo = null,
            bool isFamilyShared = false)
            : base(productId, store, expiresAtMs, purchasedAtMs, willRenew, isInGracePeriod, hasBillingIssue, isTrial, renewsTo,
                isFamilyShared)
        {
        }

        internal static SubscriptionInfo From(EntitlementStateDto d) => d == null
            ? null
            : new SubscriptionInfo(d.ProductId, d.Store, d.ExpiresAtMs, d.PurchasedAtMs, d.WillRenew, d.InGrace, d.BillingIssue, d.Trial, d.RenewsTo,
                d.FamilyShared);
    }

    /// <summary>A subscription the player paused (Google Play): it gives no access until the store resumes it, so it is
    /// not in <see cref="CustomerInfo.Subscriptions"/>. Show it as "paused until <see cref="ResumesAt"/>".</summary>
    public sealed class PausedSubscriptionInfo
    {
        /// <summary>Catalog product id (the config's product <c>id</c>) of the subscription.</summary>
        public string ProductId { get; }

        /// <summary>Store of the subscription (<c>google</c>).</summary>
        public string Store { get; }

        /// <summary>When the store resumes the subscription (UTC); null when the server does not know.</summary>
        public DateTime? ResumesAt { get; }

        internal long ResumesAtMs { get; }

        /// <summary>Creates the state (tests, custom caches). <paramref name="resumesAtMs"/>: Unix milliseconds, 0 = unknown.</summary>
        public PausedSubscriptionInfo(string productId, string store, long resumesAtMs)
        {
            ProductId = productId ?? "";
            Store = store ?? "";
            ResumesAtMs = Math.Max(0, resumesAtMs);
            ResumesAt = ResumesAtMs == 0 ? (DateTime?)null : DateTimeOffset.FromUnixTimeMilliseconds(ResumesAtMs).UtcDateTime;
        }

        internal bool SameAs(PausedSubscriptionInfo other) =>
            other != null && ProductId == other.ProductId && Store == other.Store && ResumesAtMs == other.ResumesAtMs;

        internal static PausedSubscriptionInfo From(PausedStateDto d) =>
            d == null ? null : new PausedSubscriptionInfo(d.ProductId, d.Store, d.ResumesAtMs);

        internal PausedStateDto ToDto() => new PausedStateDto { ProductId = ProductId, Store = Store, ResumesAtMs = ResumesAtMs };

        /// <inheritdoc/>
        public override string ToString() => $"{ProductId} ({Store}) paused until {(ResumesAt?.ToString("u") ?? "it is resumed")}";
    }

    /// <summary>
    /// What the player has according to the billing server, across all stores of the app: access levels, active
    /// subscriptions and owned non-consumables. Only items that give access when the server answered are listed.
    /// Keys of <see cref="Subscriptions"/> and <see cref="NonConsumables"/> are catalog product ids (the config's
    /// product <c>id</c>); <see cref="IBillingService.IsActive"/> also matches a configured product's store ids.
    /// </summary>
    public sealed class CustomerInfo
    {
        /// <summary>A device clock this much behind <see cref="FetchedAtUtc"/> is a clock set back, not drift.</summary>
        private static readonly TimeSpan ClockSkewTolerance = TimeSpan.FromHours(1);

        /// <summary>Nothing (no user, or not loaded yet).</summary>
        public static readonly CustomerInfo Empty = new CustomerInfo(null, null, null, null, 0, DateTime.MinValue, true);

        /// <summary>Billing user this belongs to.</summary>
        public string UserId { get; }

        /// <summary>Access level name → its state.</summary>
        public IReadOnlyDictionary<string, AccessInfo> Access { get; }

        /// <summary>Catalog product id → state of the active subscription.</summary>
        public IReadOnlyDictionary<string, SubscriptionInfo> Subscriptions { get; }

        /// <summary>Owned non-consumable catalog product ids.</summary>
        public IReadOnlyCollection<string> NonConsumables { get; }

        /// <summary>Catalog product id → a subscription the player paused (Google Play). It gives no access until the
        /// store resumes it; empty when nothing is paused.</summary>
        public IReadOnlyDictionary<string, PausedSubscriptionInfo> Paused { get; }

        /// <summary>Server clock when the data was produced (Unix ms).</summary>
        public long ServerTimeMs { get; }

        /// <summary>When the device received the data from the server (device clock, UTC).</summary>
        public DateTime FetchedAtUtc { get; }

        /// <summary>
        /// The server's answer for this player (fresh, or the offline cache of one); false for <see cref="Empty"/> before
        /// the first answer. Check it before acting on what the player does not own: an unknown player may own no-ads.
        /// </summary>
        public bool IsKnown => FetchedAtUtc > DateTime.MinValue;

        /// <summary>True when the server could not be reached and this is the last saved copy.</summary>
        public bool IsFromCache { get; }

        private readonly HashSet<string> m_NonConsumables;

        /// <summary>Creates customer info (tests, custom caches).</summary>
        public CustomerInfo(string userId, IDictionary<string, AccessInfo> access, IDictionary<string, SubscriptionInfo> subscriptions,
            IEnumerable<string> nonConsumables, long serverTimeMs, DateTime fetchedAtUtc, bool isFromCache,
            IDictionary<string, PausedSubscriptionInfo> paused = null)
        {
            var kept = new Dictionary<string, PausedSubscriptionInfo>();
            if (paused != null)
                foreach (var kv in paused)
                    if (!string.IsNullOrEmpty(kv.Key) && kv.Value != null)
                        kept[kv.Key] = kv.Value;
            Paused = kept;
            UserId = userId;
            Access = Clean(access);
            Subscriptions = Clean(subscriptions);
            m_NonConsumables = new HashSet<string>((nonConsumables ?? new string[0]).Where(id => !string.IsNullOrEmpty(id)));
            NonConsumables = m_NonConsumables;
            ServerTimeMs = serverTimeMs;
            FetchedAtUtc = fetchedAtUtc;
            IsFromCache = isFromCache;
        }

        private static Dictionary<string, T> Clean<T>(IDictionary<string, T> items) where T : EntitlementInfo
        {
            var result = new Dictionary<string, T>();
            if (items != null)
                foreach (var kv in items)
                    if (!string.IsNullOrEmpty(kv.Key) && kv.Value != null)
                        result[kv.Key] = kv.Value;
            return result;
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
        /// Whether items that expire can still be relied on at <paramref name="utcNow"/>: the data was received at most
        /// <paramref name="maxAge"/> ago by the device clock, and the clock was not set back before it was received.
        /// <see cref="IBillingService.IsActive"/>, <see cref="IBillingService.HasAccess"/> and
        /// <see cref="IBillingService.GetExpiration"/> ignore expiring items of stale data
        /// (<see cref="BillingConfig.entitlementsMaxAgeDays"/>) until the next refresh from the server.
        /// </summary>
        public bool IsFresh(DateTime utcNow, TimeSpan maxAge)
        {
            var age = utcNow - FetchedAtUtc;
            return age >= -ClockSkewTolerance && age <= maxAge;
        }

        /// <summary>Owns this non-consumable (catalog id).</summary>
        public bool Owns(string productId) => productId != null && m_NonConsumables.Contains(productId);

        /// <summary>Has this subscription and it has not expired by the estimated server time.</summary>
        public bool IsSubscriptionActive(string productId, DateTime utcNow) => IsSubscriptionActive(productId, utcNow, true);

        /// <summary>Owned non-consumable or active subscription.</summary>
        public bool IsActive(string productId, DateTime utcNow) => Owns(productId) || IsSubscriptionActive(productId, utcNow);

        /// <summary>Has this access level and it has not expired by the estimated server time.</summary>
        public bool HasAccess(string accessLevel, DateTime utcNow) => HasAccess(accessLevel, utcNow, true);

        /// <summary>Subscription expiration in UTC; null when not subscribed or never expires.</summary>
        public DateTime? GetExpiration(string productId) =>
            productId != null && Subscriptions.TryGetValue(productId, out var s) ? s.ExpiresAt : null;

        /// <param name="trusted">Expiring items count (the data is fresh); lifetime ones always do.</param>
        internal bool IsSubscriptionActive(string productId, DateTime utcNow, bool trusted) =>
            productId != null && Subscriptions.TryGetValue(productId, out var s) && IsLive(s, utcNow, trusted);

        internal bool HasAccess(string accessLevel, DateTime utcNow, bool trusted) =>
            accessLevel != null && Access.TryGetValue(accessLevel, out var a) && IsLive(a, utcNow, trusted);

        private bool IsLive(EntitlementInfo item, DateTime utcNow, bool trusted) =>
            item.IsLifetime || (trusted && item.ExpiresAtMs > EstimatedServerNowMs(utcNow));

        /// <summary>Same user and the same items (times of the fetch and <see cref="IsFromCache"/> aside).</summary>
        internal bool HasSameContent(CustomerInfo other)
        {
            if (other == null || UserId != other.UserId || !m_NonConsumables.SetEquals(other.m_NonConsumables))
                return false;
            if (Paused.Count != other.Paused.Count)
                return false;
            foreach (var kv in Paused)
                if (!other.Paused.TryGetValue(kv.Key, out var paused) || !kv.Value.SameAs(paused))
                    return false;
            return SameItems(Access, other.Access) && SameItems(Subscriptions, other.Subscriptions);
        }

        private static bool SameItems<T>(IReadOnlyDictionary<string, T> a, IReadOnlyDictionary<string, T> b) where T : EntitlementInfo
        {
            if (a.Count != b.Count)
                return false;
            foreach (var kv in a)
                if (!b.TryGetValue(kv.Key, out var other) || !kv.Value.SameAs(other))
                    return false;
            return true;
        }

        internal CustomerInfo AsCached() =>
            new CustomerInfo(UserId, Access.ToDictionary(kv => kv.Key, kv => kv.Value), Subscriptions.ToDictionary(kv => kv.Key, kv => kv.Value),
                m_NonConsumables, ServerTimeMs, FetchedAtUtc, true, Paused.ToDictionary(kv => kv.Key, kv => kv.Value));

        internal static CustomerInfo FromDto(string userId, CustomerInfoDto dto, DateTime fetchedAtUtc, bool fromCache) =>
            new CustomerInfo(userId,
                dto.Access?.ToDictionary(kv => kv.Key, kv => AccessInfo.From(kv.Value)),
                dto.Subscriptions?.ToDictionary(kv => kv.Key, kv => SubscriptionInfo.From(kv.Value)),
                dto.NonConsumables, dto.ServerTimeMs, fetchedAtUtc, fromCache,
                dto.Paused?.ToDictionary(kv => kv.Key, kv => PausedSubscriptionInfo.From(kv.Value)));

        internal CustomerInfoDto ToDto() => new CustomerInfoDto
        {
            UserId = UserId,
            ServerTimeMs = ServerTimeMs,
            Access = Access.ToDictionary(kv => kv.Key, kv => kv.Value.ToDto()),
            Subscriptions = Subscriptions.ToDictionary(kv => kv.Key, kv => kv.Value.ToDto()),
            NonConsumables = m_NonConsumables.ToList(),
            Paused = Paused.Count == 0 ? null : Paused.ToDictionary(kv => kv.Key, kv => kv.Value.ToDto()),
        };

        /// <summary>
        /// Local grant of the Editor without the server (<see cref="EditorVerificationMode.SkipServer"/>): a subscription
        /// for 30 days or a non-consumable, and the product's <see cref="BillingProductConfig.access"/> levels.
        /// </summary>
        internal CustomerInfo WithLocalGrant(string userId, BillingProductConfig product, DateTime utcNow)
        {
            var nowMs = new DateTimeOffset(utcNow).ToUnixTimeMilliseconds();
            var keep = userId == UserId;
            var access = keep ? Access.ToDictionary(kv => kv.Key, kv => kv.Value) : new Dictionary<string, AccessInfo>();
            var subs = keep ? Subscriptions.ToDictionary(kv => kv.Key, kv => kv.Value) : new Dictionary<string, SubscriptionInfo>();
            var owned = keep ? new HashSet<string>(m_NonConsumables) : new HashSet<string>();
            long expires;
            switch (product.type)
            {
                case BillingProductType.Subscription:
                    expires = new DateTimeOffset(utcNow.AddDays(30)).ToUnixTimeMilliseconds();
                    subs[product.id] = new SubscriptionInfo(product.id, "local", expires, nowMs, willRenew: true);
                    break;
                case BillingProductType.NonConsumable:
                    expires = 0;
                    owned.Add(product.id);
                    break;
                default:
                    return this;
            }
            foreach (var level in product.AccessLevels())
                if (!access.TryGetValue(level, out var current) || (!current.IsLifetime && (expires == 0 || expires > current.ExpiresAtMs)))
                    access[level] = new AccessInfo(product.id, "local", expires, nowMs, willRenew: expires != 0);
            return new CustomerInfo(userId, access, subs, owned, nowMs, utcNow, false,
                keep ? Paused.ToDictionary(kv => kv.Key, kv => kv.Value) : null);
        }
    }
}
