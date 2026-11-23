using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser
{
    // Initialization & user
    public sealed partial class BillingService
    {
        /// <inheritdoc/>
        /// <exception cref="BillingException">The platform has no in-app purchases, the server URL is not https outside
        /// the Editor, or the store failed to connect.</exception>
        public Task InitializeAsync(CancellationToken ct = default)
        {
            if (m_InitTask == null || m_InitTask.IsFaulted || m_InitTask.IsCanceled)
                m_InitTask = InitializeCoreAsync(ct);
            return m_InitTask;
        }

        private async Task InitializeCoreAsync(CancellationToken ct)
        {
            ThrowIfDisposed();
            foreach (var problem in m_Config.Validate())
                m_Log.Warning("Config: " + problem);
            var insecure = UrlPolicy.ServerUrlProblem(m_Config.serverBaseUrl, m_IsEditor);
            if (insecure != null)
                throw new BillingException(insecure);
            if (Platform == BillingPlatform.Unsupported && !m_IsEditor)
                throw new BillingException($"In-app purchases are not supported on {Application.platform}");

            SyncUser(refreshOnChange: false);
            BindAccount();
            await m_Store.ConnectAsync(ct);
            ThrowIfDisposed();
            var products = m_Config.products.Where(p => p != null && !string.IsNullOrEmpty(p.id)).ToList();
            m_Products = (await m_Store.FetchProductsAsync(products, ct)).Select(WithFallbacks).ToList();
            ThrowIfDisposed();
            m_Log.Info($"Initialized: {m_Products.Count} products, store {m_Store.Store} ({AppId})");

            SyncUser(refreshOnChange: false);
            m_Refresher.LoadCached(m_Context.User);
            if (m_Context.Attribution is AppleAdsAttributionProvider appleAds)
                LogFailure(appleAds.WarmUpAsync(), "Apple Ads attribution");
            IsInitialized = true;
            m_EverInitialized = true;
            if (m_CreateLifecycleHook && m_Hook == null)
                m_Hook = BillingLifecycleHook.Create(this);
            Initialized?.Invoke();

            m_Orders.ProcessWaitingOrders();
            if (m_Config.refreshEntitlementsOnInit && m_Context.User != null)
                m_Refresher.RefreshInBackground();
        }

        /// <summary>The config's fallback price and subscription period where the store gave none.</summary>
        private StoreProduct WithFallbacks(StoreProduct p)
        {
            if (p == null)
                return null;
            var config = m_Config.FindById(p.Id);
            if (config == null)
                return p;
            var price = p.Price <= 0 && string.IsNullOrEmpty(p.LocalizedPrice) && config.HasFallbackPrice;
            var period = p.Type == BillingProductType.Subscription && p.SubscriptionPeriod.Length == 0 && config.HasFallbackPeriod;
            if (!price && !period)
                return p;
            return new StoreProduct(p.Id, p.StoreId, p.Type,
                price ? config.FallbackPriceText : p.LocalizedPrice,
                price ? (decimal)config.fallbackPrice : p.Price,
                price ? config.fallbackCurrency.ToUpperInvariant() : p.Currency,
                p.Title, p.Description, p.AvailableToPurchase, p.Native, price || p.IsFallbackPrice,
                period ? config.fallbackPeriod : p.SubscriptionPeriod, period || p.IsFallbackPeriod,
                p.Offers, p.IntroEligible);
        }

        private async void LogFailure(Task task, string what)
        {
            try
            {
                await task;
            }
            catch (Exception e)
            {
                m_Log.Warning($"{what} failed: {e.Message}");
            }
        }

        /// <inheritdoc/>
        public void SetRefundDataConsent(bool consented)
        {
            m_Storage.Save(BillingContext.RefundConsentKey, consented ? "true" : "false");
        }

        /// <inheritdoc/>
        public async Task<bool> SetCustomAttributesAsync(IDictionary<string, object> attributes, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            var user = SyncUser();
            if (user == null || attributes == null || attributes.Count == 0)
                return false;
            var response = await m_Api.AttributesAsync(new AttributesRequest
            {
                AppId = AppId,
                UserId = user,
                Attributes = new Dictionary<string, object>(attributes),
                Client = m_Client,
            }, ct);
            if (!response.IsHttpSuccess)
                m_Log.Warning($"Attributes not saved: {response.Value?.Message ?? response.ToString()}");
            return response.IsHttpSuccess;
        }

        /// <inheritdoc/>
        public void SetUserId(string userId)
        {
            m_Users = new StaticUserIdProvider(userId);
            SyncUser();
        }

        /// <summary>The user id of the provider; null when it has none or failed.</summary>
        private string ReadUser()
        {
            try
            {
                var id = m_Users?.GetUserId();
                return string.IsNullOrEmpty(id) ? null : id;
            }
            catch (Exception e)
            {
                m_Log.Error("User id provider failed: " + e.Message);
                return null;
            }
        }

        /// <summary>
        /// Reads the user id; when it changed since the last read, loads the new player's cached customer info, binds
        /// purchases to them, verifies orders that waited for a player and (with <paramref name="refreshOnChange"/>)
        /// refreshes the customer info. Returns the current user id.
        /// </summary>
        private string SyncUser(bool refreshOnChange = true)
        {
            var current = ReadUser();
            if (current == m_Context.User)
                return current;
            m_Context.User = current;
            m_Log.Info(current == null ? "The player signed out." : "Player " + current);
            m_Refresher.LoadCached(current);
            BindAccount();
            m_Orders.ProcessWaitingOrders();
            if (refreshOnChange && IsInitialized && current != null)
                m_Refresher.RefreshInBackground();
            return current;
        }

        private void BindAccount()
        {
            var user = m_Context.User;
            if (!m_Config.bindPurchasesToUser || user == null || user == m_BoundUser)
                return;
            m_Store.SetAccountBinding(user);
            m_BoundUser = user;
        }

        /// <summary>The app came back to the foreground (<see cref="BillingLifecycleHook"/>): notices a changed user id
        /// and refreshes the customer info, at most once per <see cref="CustomerInfoRefresher.ResumeRefreshInterval"/>.</summary>
        internal void OnAppResumed()
        {
            if (m_Context.IsDisposed)
                return;
            var user = SyncUser();
            if (!IsInitialized || user == null || SkipServer)
                return;
            m_Refresher.RefreshOnResume();
        }

        /// <summary>Once a second in play mode: notices a changed user id of the provider.</summary>
        internal void OnTick()
        {
            if (!m_Context.IsDisposed)
                SyncUser();
        }
    }
}
