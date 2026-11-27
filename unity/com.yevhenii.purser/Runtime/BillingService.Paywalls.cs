using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Paywalls & A/B tests
    public sealed partial class BillingService
    {
        /// <inheritdoc/>
        public async Task<Paywall> GetPaywallAsync(string placementId, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            if (string.IsNullOrEmpty(placementId))
                throw new ArgumentException("Placement id is empty", nameof(placementId));
            var user = SyncUser();
            var response = await m_Api.PaywallAsync(new PaywallRequest { AppId = AppId, Store = BuildStore, Placement = placementId, UserId = user, Client = m_Client }, ct);
            if (response.IsHttpSuccess && response.Value != null)
            {
                var now = m_UtcNow();
                PaywallCache.Save(m_Storage, m_Context.CacheScope, user, placementId, response.Value, now, m_Log);
                return ToPaywall(placementId, response.Value, false, now);
            }

            if (response.HttpStatus == 404)
            {
                // The placement was removed or disabled on purpose: forget the cached answer.
                PaywallCache.Save(m_Storage, m_Context.CacheScope, user, placementId, null, m_UtcNow(), m_Log);
                throw new PaywallNotFoundException(placementId, response.Value?.Code ?? "ERR_PLACEMENT_NOT_FOUND",
                    response.Value?.Message ?? $"no placement '{placementId}'");
            }
            if (response.HttpStatus >= 400 && response.HttpStatus < 500 && !response.IsRateLimited)
                throw new BillingException($"Paywall request for '{placementId}' was refused: {response.Value?.Message ?? response.Error}");

            var cached = PaywallCache.Load(m_Storage, m_Context.CacheScope, user, placementId, m_Log);
            if (cached.HasValue)
            {
                m_Log.Warning($"Paywall request failed ({response}); using the cached answer for '{placementId}'.");
                return ToPaywall(placementId, cached.Value.Response, true, cached.Value.FetchedAtUtc);
            }
            throw new BillingException($"Paywall '{placementId}' is unavailable: {response}");
        }

        private Paywall ToPaywall(string placementId, PaywallResponse r, bool fromCache, DateTime fetchedAtUtc)
        {
            var products = new List<PaywallProduct>();
            var missing = new List<string>();
            foreach (var p in r.Products ?? new List<PaywallProductDto>())
            {
                var storeId = string.IsNullOrEmpty(p?.StoreId) ? p?.Id : p.StoreId;
                if (string.IsNullOrEmpty(storeId))
                    continue;
                var config = m_Config.FindByStoreId(storeId) ?? m_Config.FindById(p.Id);
                if (config == null)
                {
                    missing.Add(storeId);
                    continue;
                }
                var configStoreId = config.StoreId(m_Store.Store);
                if (products.Any(x => x.StoreId == configStoreId))
                    continue;
                products.Add(new PaywallProduct(config, configStoreId, m_Products.FirstOrDefault(sp => sp.StoreId == configStoreId)));
            }
            if (missing.Count > 0)
                m_Log.Warning($"Paywall '{placementId}': products not in BillingConfig are skipped: {string.Join(", ", missing)}");
            var remoteConfig = r.RemoteConfig == null || r.RemoteConfig.Type == Newtonsoft.Json.Linq.JTokenType.Null
                ? "{}"
                : r.RemoteConfig.ToString(Newtonsoft.Json.Formatting.None);
            return new Paywall(r.Placement ?? placementId, r.PaywallId, r.PaywallName, r.VariantId, r.TestId, products, missing,
                remoteConfig, fromCache, fetchedAtUtc);
        }

        /// <inheritdoc/>
        public async Task<bool> LogPaywallShownAsync(Paywall paywall, CancellationToken ct = default)
        {
            if (paywall == null || string.IsNullOrEmpty(paywall.VariantId) || m_Context.IsDisposed)
                return false;
            if (SkipServer)
                return false; // Editor sessions would skew the A/B test views.
            try
            {
                var response = await m_Api.PaywallEventAsync(new PaywallEventRequest
                {
                    AppId = AppId,
                    Store = BuildStore,
                    Placement = paywall.PlacementId,
                    VariantId = paywall.VariantId,
                    TestId = NullIfEmpty(paywall.TestId),
                    UserId = SyncUser(),
                    Event = PaywallEventRequest.View,
                }, ct);
                if (!response.IsHttpSuccess && m_Log.IsEnabled(BillingLogLevel.Verbose))
                    m_Log.Verbose($"Paywall view of {paywall} not recorded: {response}");
                return response.IsHttpSuccess;
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception e)
            {
                m_Log.Verbose("Paywall view not recorded: " + e.Message);
                return false;
            }
        }
    }
}
