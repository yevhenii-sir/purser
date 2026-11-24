using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>What <see cref="CustomerInfoRefresher"/> asks of the service that owns it.</summary>
    internal interface ICustomerInfoHost
    {
        /// <summary>Reads the user id again, handling a changed player first (without a refresh of the new player's
        /// customer info: the caller is refreshing); null when there is no player.</summary>
        string SyncUser();

        /// <summary>What the customer info gives changed: raises <see cref="IBillingService.CustomerInfoChanged"/>.
        /// A throwing handler is logged, never passed on.</summary>
        void OnCustomerInfoChanged(CustomerInfo info);
    }

    /// <summary>
    /// The customer info of the current player: the last answer of the server (<c>/entitlements</c>, <c>/restore</c>),
    /// cached per player for offline starts (<see cref="CustomerInfoCache"/>), refreshed on request, in the background,
    /// when the app returns to the foreground and when something in it expires. Used from the Unity main thread only.
    /// </summary>
    internal sealed class CustomerInfoRefresher
    {
        /// <summary>Returning to the foreground refreshes the customer info at most this often.</summary>
        internal static readonly TimeSpan ResumeRefreshInterval = TimeSpan.FromMinutes(5);

        /// <summary>An answer that gives the same as the cached one is written to the cache at most this often (the
        /// cache keeps when the data was received, which <see cref="BillingConfig.entitlementsMaxAgeDays"/> counts from).</summary>
        internal static readonly TimeSpan CacheRewriteInterval = TimeSpan.FromHours(1);

        /// <summary>How long after something expired (or a paused subscription resumes) the server is asked what the
        /// player has now.</summary>
        internal static readonly TimeSpan ExpiryRefreshDelay = TimeSpan.FromSeconds(5);

        /// <summary>The refresh after an expiry is repeated at most this often while the server still lists the item.</summary>
        internal static readonly TimeSpan ExpiryRefreshInterval = TimeSpan.FromMinutes(1);

        private readonly BillingContext m_Context;
        private readonly ICustomerInfoHost m_Host;
        private readonly BillingConfig m_Config;
        private readonly IBillingApi m_Api;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly Func<DateTime> m_UtcNow;

        private CustomerInfo m_CustomerInfo = CustomerInfo.Empty;
        /// <summary>When the server last answered a refresh (the foreground throttle).</summary>
        private DateTime? m_LastRefreshUtc;
        /// <summary>The fetch time of the cached copy of the current player; null when there is none.</summary>
        private DateTime? m_CachedAtUtc;

        /// <summary>A <c>/entitlements</c> request is running, for this player; its number.</summary>
        private bool m_Requesting;
        private string m_RequestUser;
        private int m_RequestId;
        private int m_Requests;
        /// <summary>Who asked while the request was running: they share the one refresh that starts when it ends. Each
        /// caller has its own task, completed from here (nothing but the caller awaits it).</summary>
        private List<TaskCompletionSource<CustomerInfo>> m_Trailing;

        /// <summary>Server time (Unix ms) of the next change the customer info itself announces: the earliest expiry or
        /// the earliest resume of a paused subscription; 0 when there is none.</summary>
        private long m_NextChangeMs;
        private DateTime? m_LastExpiryRefreshUtc;

        /// <summary>Any id of a configured product (its own or a store's) → every id of that product.</summary>
        private Dictionary<string, string[]> m_ProductIds;
        /// <summary>How many products the config had when <see cref="m_ProductIds"/> was built.</summary>
        private int m_IndexedProducts = -1;

        public CustomerInfoRefresher(BillingContext context, ICustomerInfoHost host)
        {
            m_Context = context;
            m_Host = host;
            m_Config = context.Config;
            m_Api = context.Api;
            m_Storage = context.Storage;
            m_Log = context.Log;
            m_UtcNow = context.UtcNow;
        }

        /// <summary>Last known customer info (from the server or the local cache).</summary>
        public CustomerInfo Current => m_CustomerInfo;

        /// <summary>The key cached copies must be signed with; null when signed responses are off or the Editor grants
        /// locally (its grants have no signature).</summary>
        private string CacheKey =>
            m_Context.SkipServer || string.IsNullOrWhiteSpace(m_Config.responseSigningPublicKey)
                ? null
                : m_Config.responseSigningPublicKey.Trim();

        // ------------------------------------------------------------------
        // Refresh
        // ------------------------------------------------------------------

        /// <summary>
        /// Asks the server for the customer info of the current player; the cached copy when it cannot be reached
        /// (<see cref="IBillingService.RefreshCustomerInfoAsync"/>). One request runs at a time: whoever asks while it
        /// runs gets the one refresh that follows it, so the answer is never older than the call.
        /// <paramref name="ct"/> stops waiting, not the request.
        /// </summary>
        public async Task<CustomerInfo> RefreshAsync(CancellationToken ct)
        {
            var user = m_Host.SyncUser();
            if (user == null)
                return CustomerInfo.Empty;
            if (m_Context.SkipServer)
                return m_CustomerInfo;

            var shared = Join(user);
            if (!ct.CanBeCanceled || shared.IsCompleted)
                return await shared;
            var cancelled = new TaskCompletionSource<bool>();
            using (ct.Register(() => cancelled.TrySetResult(true)))
            {
                if (await Task.WhenAny(shared, cancelled.Task) != shared)
                    throw new OperationCanceledException(ct);
            }
            return await shared;
        }

        /// <summary>The refresh a caller waits for: the one that follows the request running for the player, or a new
        /// request.</summary>
        private Task<CustomerInfo> Join(string user)
        {
            if (!m_Requesting || m_RequestUser != user)
                return RequestAsync(user);
            var waiter = new TaskCompletionSource<CustomerInfo>();
            if (m_Trailing == null)
                m_Trailing = new List<TaskCompletionSource<CustomerInfo>>();
            m_Trailing.Add(waiter);
            return waiter.Task;
        }

        private async Task<CustomerInfo> RequestAsync(string user)
        {
            var id = ++m_Requests;
            m_RequestId = id;
            m_RequestUser = user;
            m_Requesting = true;
            try
            {
                var response = await m_Api.EntitlementsAsync(
                    new EntitlementsRequest { AppId = m_Context.AppId, UserId = user, Client = m_Context.Client }, m_Context.Lifetime);
                if (response.IsHttpSuccess && response.Value != null)
                {
                    // Only an answer counts for the foreground throttle: a failed refresh is tried again on the next resume.
                    m_LastRefreshUtc = m_UtcNow();
                    Set(user, response.Value, new SignedAnswer(response.RawBody, response.Signature, false));
                    return m_CustomerInfo;
                }

                m_Log.Warning("Customer info request failed: " + response);
                return m_CustomerInfo.UserId == user ? m_CustomerInfo.AsCached() : CustomerInfo.Empty;
            }
            finally
            {
                // A request for another player may have started meanwhile: that one is the request in flight now.
                if (m_RequestId == id)
                {
                    m_Requesting = false;
                    m_RequestUser = null;
                    RefreshForTrailing();
                }
            }
        }

        /// <summary>The request ended: one more refresh for everybody who asked while it ran.</summary>
        private async void RefreshForTrailing()
        {
            var waiters = m_Trailing;
            m_Trailing = null;
            if (waiters == null)
                return;
            try
            {
                var result = m_CustomerInfo;
                if (!m_Context.IsDisposed)
                {
                    // The player may have changed meanwhile.
                    var user = m_Host.SyncUser();
                    result = user == null ? CustomerInfo.Empty : await Join(user);
                }
                foreach (var waiter in waiters)
                    waiter.TrySetResult(result);
            }
            catch (Exception e)
            {
                foreach (var waiter in waiters)
                    waiter.TrySetException(e);
            }
        }

        /// <summary>Refreshes without a caller waiting for it; a failure is logged.</summary>
        public async void RefreshInBackground()
        {
            if (m_Context.IsDisposed)
                return;
            try
            {
                await RefreshAsync(m_Context.Lifetime);
            }
            catch (OperationCanceledException)
            {
            }
            catch (Exception e)
            {
                m_Log.Warning("Customer info refresh failed: " + e.Message);
            }
        }

        /// <summary>The app came back to the foreground: refreshes in the background unless the server answered a
        /// refresh less than <see cref="ResumeRefreshInterval"/> ago.</summary>
        public void RefreshOnResume()
        {
            var now = m_UtcNow();
            if (m_LastRefreshUtc is DateTime last && now >= last && now - last < ResumeRefreshInterval)
                return;
            RefreshInBackground();
        }

        /// <summary>
        /// Called once a second: when something in the customer info expired (or a paused subscription was due to
        /// resume) <see cref="ExpiryRefreshDelay"/> ago by the server's clock, asks the server what the player has now,
        /// at most once per <see cref="ExpiryRefreshInterval"/>.
        /// </summary>
        public void RefreshAfterExpiry()
        {
            if (m_NextChangeMs <= 0)
                return;
            var now = m_UtcNow();
            if (m_CustomerInfo.EstimatedServerNowMs(now) < m_NextChangeMs + (long)ExpiryRefreshDelay.TotalMilliseconds)
                return;
            if (m_LastExpiryRefreshUtc is DateTime last && now >= last && now - last < ExpiryRefreshInterval)
                return;
            m_LastExpiryRefreshUtc = now;
            RefreshInBackground();
        }

        // ------------------------------------------------------------------
        // The current customer info
        // ------------------------------------------------------------------

        /// <summary>
        /// Keeps and caches the server's answer for <paramref name="user"/>. Dropped when the player changed while the
        /// request was running, and when it is older (by the server's clock) than the answer already shown: answers
        /// may arrive out of order.
        /// </summary>
        /// <param name="signed">The answer's bytes and signature, kept in the cache (signed responses).</param>
        public void Set(string user, CustomerInfoDto dto, SignedAnswer signed = default)
        {
            if (user != m_Context.User)
                return; // the player changed while the request was running
            var current = m_CustomerInfo;
            if (!current.IsFromCache && current.UserId == user && dto.ServerTimeMs > 0 && dto.ServerTimeMs < current.ServerTimeMs)
            {
                if (m_Log.IsEnabled(BillingLogLevel.Verbose))
                    m_Log.Verbose($"A customer info answer older than the current one is dropped ({dto.ServerTimeMs} < {current.ServerTimeMs}).");
                return;
            }
            var now = m_UtcNow();
            var info = CustomerInfo.FromDto(user, dto, now, false);
            // The same content as the cached copy is not written again for a while: only its fetch time would change.
            var cachedLately = m_CachedAtUtc is DateTime cachedAt && now >= cachedAt && now - cachedAt < CacheRewriteInterval;
            if (!cachedLately || current.UserId != user || !info.HasSameContent(current))
            {
                CustomerInfoCache.Save(m_Storage, m_Context.AppId, user, dto, now, m_Log, signed);
                m_CachedAtUtc = now;
            }
            Set(info);
        }

        /// <summary>Keeps <paramref name="info"/> and tells the host (<see cref="ICustomerInfoHost.OnCustomerInfoChanged"/>)
        /// when what it gives differs from the previous one.</summary>
        private void Set(CustomerInfo info)
        {
            var changed = !info.HasSameContent(m_CustomerInfo);
            m_CustomerInfo = info;
            m_NextChangeMs = NextChange(info);
            if (!changed)
                return;
            m_Host.OnCustomerInfoChanged(info);
        }

        /// <summary>The earliest expiry or resume time in <paramref name="info"/> (Unix ms); 0 when nothing in it ends.</summary>
        private static long NextChange(CustomerInfo info)
        {
            long next = 0;
            void Consider(long ms)
            {
                if (ms > 0 && (next == 0 || ms < next))
                    next = ms;
            }
            foreach (var access in info.Access.Values)
                Consider(access.ExpiresAtMs);
            foreach (var subscription in info.Subscriptions.Values)
                Consider(subscription.ExpiresAtMs);
            foreach (var paused in info.Paused.Values)
                Consider(paused.ResumesAtMs);
            return next;
        }

        /// <summary>Shows the cached customer info of <paramref name="user"/>; nothing when there is no player or no
        /// cached copy that can be trusted (with signed responses: one whose signature is valid).</summary>
        public void LoadCached(string user)
        {
            var cached = user == null ? null : CustomerInfoCache.Load(m_Storage, m_Context.AppId, user, m_Log, CacheKey);
            m_CachedAtUtc = cached?.FetchedAtUtc;
            Set(cached ?? CustomerInfo.Empty);
        }

        /// <summary>Editor without the server: grants the product and its access levels, cached like a server answer
        /// so the next Editor session has them too.</summary>
        public void GrantLocally(string user, BillingProductConfig config)
        {
            if (user == null)
                return;
            var info = m_CustomerInfo.WithLocalGrant(user, config, m_UtcNow());
            if (ReferenceEquals(info, m_CustomerInfo))
                return;
            CustomerInfoCache.Save(m_Storage, m_Context.AppId, user, info.ToDto(), info.FetchedAtUtc, m_Log);
            m_CachedAtUtc = info.FetchedAtUtc;
            Set(info);
        }

        // ------------------------------------------------------------------
        // What the player has
        // ------------------------------------------------------------------

        /// <summary>Expiring items of the customer info count (<see cref="BillingConfig.entitlementsMaxAgeDays"/>).</summary>
        private bool IsTrusted(CustomerInfo info, DateTime now)
        {
            var maxAgeDays = m_Config.entitlementsMaxAgeDays;
            return maxAgeDays <= 0 || info.IsFresh(now, TimeSpan.FromDays(maxAgeDays));
        }

        /// <summary><see cref="IBillingService.IsActive"/>.</summary>
        public bool IsActive(string productId)
        {
            var now = m_UtcNow();
            var info = m_CustomerInfo;
            var trusted = IsTrusted(info, now);
            if (!ProductIds().TryGetValue(productId ?? "", out var ids))
                return info.Owns(productId) || info.IsSubscriptionActive(productId, now, trusted);
            foreach (var id in ids)
                if (info.Owns(id) || info.IsSubscriptionActive(id, now, trusted))
                    return true;
            return false;
        }

        /// <summary><see cref="IBillingService.HasAccess"/>.</summary>
        public bool HasAccess(string accessLevel)
        {
            if (string.IsNullOrEmpty(accessLevel))
                return false;
            var now = m_UtcNow();
            var info = m_CustomerInfo;
            return info.HasAccess(accessLevel, now, IsTrusted(info, now));
        }

        /// <summary><see cref="IBillingService.GetExpiration"/>.</summary>
        public DateTime? GetExpiration(string productId)
        {
            var now = m_UtcNow();
            var info = m_CustomerInfo;
            var trusted = IsTrusted(info, now);
            if (!ProductIds().TryGetValue(productId ?? "", out var ids))
                return info.IsSubscriptionActive(productId, now, trusted) ? info.GetExpiration(productId) : null;
            DateTime? latest = null;
            foreach (var id in ids)
            {
                if (!info.IsSubscriptionActive(id, now, trusted))
                    continue;
                var expires = info.GetExpiration(id);
                if (expires.HasValue && (latest == null || expires.Value > latest.Value))
                    latest = expires;
            }
            return latest;
        }

        /// <summary>
        /// Builds the lookup of <see cref="IsActive"/> and <see cref="GetExpiration"/> from the config's products: an id
        /// names the product that has it as its own id, otherwise the first product that has it as a store id. Built
        /// when the service initializes, at the first question before that and when products were added to or removed
        /// from the config, not for every question.
        /// </summary>
        public void IndexProducts()
        {
            var index = new Dictionary<string, string[]>();
            var products = (m_Config.products ?? new List<BillingProductConfig>()).Where(p => p != null).ToList();
            var ids = products.Select(p => p.AllIds().ToArray()).ToList();
            for (var i = 0; i < products.Count; i++)
                if (!string.IsNullOrEmpty(products[i].id) && !index.ContainsKey(products[i].id))
                    index[products[i].id] = ids[i];
            for (var i = 0; i < products.Count; i++)
                foreach (var id in ids[i])
                    if (!index.ContainsKey(id))
                        index[id] = ids[i];
            m_ProductIds = index;
            m_IndexedProducts = m_Config.products?.Count ?? 0;
        }

        private Dictionary<string, string[]> ProductIds()
        {
            // Products merged into the config later (BillingConfig.MergeJson) are picked up.
            if (m_ProductIds == null || m_IndexedProducts != (m_Config.products?.Count ?? 0))
                IndexProducts();
            return m_ProductIds;
        }
    }
}
