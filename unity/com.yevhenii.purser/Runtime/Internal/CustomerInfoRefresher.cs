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
    /// cached per player for offline starts (<see cref="CustomerInfoCache"/>), refreshed on request, in the background
    /// and when the app returns to the foreground. Used from the Unity main thread only.
    /// </summary>
    internal sealed class CustomerInfoRefresher
    {
        /// <summary>Returning to the foreground refreshes the customer info at most this often.</summary>
        internal static readonly TimeSpan ResumeRefreshInterval = TimeSpan.FromMinutes(5);

        private readonly BillingContext m_Context;
        private readonly ICustomerInfoHost m_Host;
        private readonly BillingConfig m_Config;
        private readonly IBillingApi m_Api;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly Func<DateTime> m_UtcNow;

        private CustomerInfo m_CustomerInfo = CustomerInfo.Empty;
        private DateTime? m_LastRefreshUtc;

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

        // ------------------------------------------------------------------
        // Refresh
        // ------------------------------------------------------------------

        /// <summary>Asks the server for the customer info of the current player; the cached copy when it cannot be
        /// reached (<see cref="IBillingService.RefreshCustomerInfoAsync"/>).</summary>
        public async Task<CustomerInfo> RefreshAsync(CancellationToken ct)
        {
            var user = m_Host.SyncUser();
            if (user == null)
                return CustomerInfo.Empty;
            if (m_Context.SkipServer)
                return m_CustomerInfo;

            m_LastRefreshUtc = m_UtcNow();
            var response = await m_Api.EntitlementsAsync(
                new EntitlementsRequest { AppId = m_Context.AppId, UserId = user, Client = m_Context.Client }, ct);
            if (response.IsHttpSuccess && response.Value != null)
            {
                Set(user, response.Value);
                return m_CustomerInfo;
            }

            m_Log.Warning("Customer info request failed: " + response);
            return m_CustomerInfo.UserId == user ? m_CustomerInfo.AsCached() : CustomerInfo.Empty;
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

        /// <summary>The app came back to the foreground: refreshes in the background unless a refresh started less
        /// than <see cref="ResumeRefreshInterval"/> ago.</summary>
        public void RefreshOnResume()
        {
            var now = m_UtcNow();
            if (m_LastRefreshUtc is DateTime last && now >= last && now - last < ResumeRefreshInterval)
                return;
            RefreshInBackground();
        }

        // ------------------------------------------------------------------
        // The current customer info
        // ------------------------------------------------------------------

        /// <summary>Keeps and caches the server's answer for <paramref name="user"/>; dropped when the player changed
        /// while the request was running.</summary>
        public void Set(string user, CustomerInfoDto dto)
        {
            if (user != m_Context.User)
                return; // the player changed while the request was running
            var now = m_UtcNow();
            CustomerInfoCache.Save(m_Storage, m_Context.AppId, user, dto, now, m_Log);
            Set(CustomerInfo.FromDto(user, dto, now, false));
        }

        /// <summary>Keeps <paramref name="info"/> and tells the host (<see cref="ICustomerInfoHost.OnCustomerInfoChanged"/>)
        /// when what it gives differs from the previous one.</summary>
        private void Set(CustomerInfo info)
        {
            var changed = !info.HasSameContent(m_CustomerInfo);
            m_CustomerInfo = info;
            if (!changed)
                return;
            m_Host.OnCustomerInfoChanged(info);
        }

        /// <summary>Shows the cached customer info of <paramref name="user"/>; nothing when there is no player or no
        /// cached copy.</summary>
        public void LoadCached(string user)
        {
            var cached = user == null ? null : CustomerInfoCache.Load(m_Storage, m_Context.AppId, user, m_Log);
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
            return ProductIds(productId).Any(id => info.Owns(id) || info.IsSubscriptionActive(id, now, trusted));
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
            return ProductIds(productId)
                .Where(id => info.IsSubscriptionActive(id, now, trusted))
                .Select(id => info.GetExpiration(id))
                .Where(d => d.HasValue)
                .OrderByDescending(d => d.Value)
                .FirstOrDefault();
        }

        /// <summary>Every id of the configured product named by its id or any store id; the id alone otherwise.</summary>
        private IEnumerable<string> ProductIds(string productId)
        {
            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId);
            if (config == null)
                return new[] { productId };
            return config.AllIds();
        }
    }
}
