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
        /// <remarks>When the store cannot be connected or the products cannot be loaded, the returned task fails and
        /// the service keeps trying in the background (with the backoff of a store reconnection, and right away when the
        /// app returns to the foreground) until it succeeds or is disposed; <see cref="Initialized"/> is raised then.
        /// A configuration problem is not retried.</remarks>
        public Task InitializeAsync(CancellationToken ct = default)
        {
            // One attempt at a time: the one that runs (also a background one), or the one that succeeded.
            if (m_InitTask != null && !m_InitTask.IsFaulted && !m_InitTask.IsCanceled)
                return m_InitTask;
            try
            {
                ThrowIfDisposed();
                CheckConfiguration();
            }
            catch (Exception e)
            {
                // Nothing a later attempt could change: not retried in the background.
                return m_InitTask = Task.FromException(e);
            }

            // Before the store is connected: the app returning to the foreground retries a failed initialization.
            if (m_CreateLifecycleHook && m_Hook == null)
                m_Hook = BillingLifecycleHook.Create(this);
            var attempt = ConnectAndLoadAsync(ct);
            m_InitTask = attempt;
            RetryInBackgroundIfFailed(attempt, ct);
            return attempt;
        }

        /// <summary>Problems of the config and the platform that no retry fixes.</summary>
        /// <exception cref="BillingException">The server URL is not https outside the Editor, or the platform has no
        /// in-app purchases.</exception>
        private void CheckConfiguration()
        {
            foreach (var problem in m_Config.Validate())
                m_Log.Warning("Config: " + problem);
            var insecure = UrlPolicy.ServerUrlProblem(m_Config.serverBaseUrl, m_IsEditor);
            if (insecure != null)
                throw new BillingException(insecure);
            if (Platform == BillingPlatform.Unsupported && !m_IsEditor)
                throw new BillingException($"In-app purchases are not supported on {Application.platform}");
        }

        /// <summary>One initialization attempt: connects the store, loads the products, then tells the game.</summary>
        private async Task ConnectAndLoadAsync(CancellationToken ct)
        {
            ThrowIfDisposed();
            // Orders the store delivers from now on wait for the prices (IOrderProcessorHost.ProductsFetch).
            var fetch = m_ProductsFetch = new TaskCompletionSource<bool>();
            try
            {
                SyncUser(refreshOnChange: false);
                BindAccount();
                await m_Store.ConnectAsync(ct);
                ThrowIfDisposed();
                var products = m_Config.products.Where(p => p != null && !string.IsNullOrEmpty(p.id)).ToList();
                m_Products = (await m_Store.FetchProductsAsync(products, ct)).Select(WithFallbacks).ToList();
                IndexProducts();
            }
            finally
            {
                fetch.TrySetResult(true);
            }
            ThrowIfDisposed();
            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info($"Initialized: {m_Products.Count} products, store {m_Store.Store} ({AppId})");

            // A player who appeared meanwhile; their cached customer info is loaded when the player changes.
            SyncUser(refreshOnChange: false);
            if (m_Context.Attribution is AppleAdsAttributionProvider appleAds)
                LogFailure(appleAds.WarmUpAsync(), "Apple Ads attribution");
            IsInitialized = true;
            m_EverInitialized = true;
            try
            {
                Initialized?.Invoke();
            }
            catch (Exception e)
            {
                m_Log.Error($"{nameof(Initialized)} handler threw: {e}");
            }

            m_Orders.ProcessWaitingOrders();
            m_Observed.RetryAll();
            if (m_Config.refreshEntitlementsOnInit && m_Context.User != null)
                m_Refresher.RefreshInBackground();
        }

        /// <summary>Starts the background retries when <paramref name="attempt"/> fails for a reason a later attempt
        /// may not have (the store, the network); not when the caller cancelled it or the service was disposed.</summary>
        private async void RetryInBackgroundIfFailed(Task attempt, CancellationToken ct)
        {
            try
            {
                await attempt;
                return;
            }
            catch (Exception e)
            {
                if (m_Context.IsDisposed || m_InitRetrying || m_EverInitialized)
                    return;
                if (e is OperationCanceledException && ct.IsCancellationRequested)
                    return;
                m_Log.Warning($"Initialization failed ({e.Message}); retrying in the background.");
            }
            await RetryInitializationAsync();
        }

        /// <summary>Attempts the initialization again until it succeeds or the service is disposed, waiting
        /// <see cref="ReconnectDelay"/> before each attempt; <see cref="OnAppResumed"/> ends the wait.</summary>
        private async Task RetryInitializationAsync()
        {
            m_InitRetrying = true;
            try
            {
                for (var attempt = 0; !m_Context.IsDisposed && !m_EverInitialized; attempt++)
                {
                    m_InitWake = new TaskCompletionSource<bool>();
                    using (var wait = CancellationTokenSource.CreateLinkedTokenSource(m_Context.Lifetime))
                    {
                        await Task.WhenAny(m_Delay(ReconnectDelay(attempt), wait.Token), m_InitWake.Task);
                        wait.Cancel();
                    }
                    if (m_Context.IsDisposed || m_EverInitialized)
                        return;
                    // An InitializeAsync call of the game may be trying right now: that attempt is the one to wait for.
                    if (m_InitTask == null || m_InitTask.IsCompleted)
                        m_InitTask = ConnectAndLoadAsync(m_Context.Lifetime);
                    try
                    {
                        await m_InitTask;
                    }
                    catch (Exception e)
                    {
                        if (m_Context.IsDisposed)
                            return;
                        m_Log.Warning($"Initialization attempt {attempt + 2} failed: {e.Message}");
                    }
                }
            }
            catch (Exception e)
            {
                if (!m_Context.IsDisposed)
                    m_Log.Error("Initialization retries stopped: " + e);
            }
            finally
            {
                m_InitRetrying = false;
                m_InitWake = null;
            }
        }

        /// <summary>Builds the lookups of <see cref="GetProduct"/>, <see cref="IsActive"/> and
        /// <see cref="GetExpiration"/> once, instead of searching the lists for every question.</summary>
        private void IndexProducts()
        {
            var byId = new Dictionary<string, StoreProduct>();
            var byStoreId = new Dictionary<string, StoreProduct>();
            foreach (var product in m_Products)
            {
                if (product == null)
                    continue;
                if (product.Id != null && !byId.ContainsKey(product.Id))
                    byId[product.Id] = product;
                if (product.StoreId != null && !byStoreId.ContainsKey(product.StoreId))
                    byStoreId[product.StoreId] = product;
            }
            m_ProductsById = byId;
            m_ProductsByStoreId = byStoreId;
            m_Refresher.IndexProducts();
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
            FlushStorage();
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
                Store = BuildStore,
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
            if (!string.IsNullOrEmpty(userId) && !AccountIds.IsValidUserId(userId))
                throw new ArgumentException($"user id '{userId}' is not accepted by the server: use 1-128 ASCII letters, digits and - _ . : " +
                                            "(hash an e-mail address, for example)", nameof(userId));
            m_Users = new StaticUserIdProvider(userId);
            SyncUser();
        }

        /// <summary>The user id of the provider; null when it has none or failed.</summary>
        private string ReadUser()
        {
            try
            {
                var id = m_Users?.GetUserId();
                if (string.IsNullOrEmpty(id))
                    return null;
                if (!AccountIds.IsValidUserId(id))
                {
                    // The server would refuse every request for it: no player rather than purchases retried forever.
                    m_Log.Error($"User id '{id}' is not accepted by the server (1-128 ASCII letters, digits and - _ . :): purchases " +
                                "wait until the provider returns a valid id.");
                    return null;
                }
                return id;
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
            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info(current == null ? "The player signed out." : "Player " + current);
            m_Refresher.LoadCached(current);
            BindAccount();
            m_Orders.ProcessWaitingOrders();
            if (IsInitialized)
                m_Observed.RetryAll();
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

        /// <summary>The app came back to the foreground (<see cref="BillingLifecycleHook"/>): notices a changed user id,
        /// retries a failed initialization now, verifies again the orders that were given up and refreshes the customer
        /// info, at most once per <see cref="CustomerInfoRefresher.ResumeRefreshInterval"/>.</summary>
        internal void OnAppResumed()
        {
            if (m_Context.IsDisposed)
                return;
            var previous = m_Context.User;
            var user = SyncUser(refreshOnChange: false);
            m_InitWake?.TrySetResult(true);
            if (!IsInitialized || user == null || SkipServer)
                return;
            m_Orders.RetryGivenUp();
            m_Observed.RetryAll();
            // One refresh: a new player's right away, the same player's unless the server answered one lately.
            if (user != previous)
                m_Refresher.RefreshInBackground();
            else
                m_Refresher.RefreshOnResume();
        }

        /// <summary>The app goes to the background or quits (<see cref="BillingLifecycleHook"/>): what the storage
        /// batched is written to disk, as the app may not come back.</summary>
        internal void OnAppPaused()
        {
            if (!m_Context.IsDisposed)
                FlushStorage();
        }

        /// <summary>Once a second in play mode: notices a changed user id of the provider, and asks the server what the
        /// player has when something in the customer info just expired.</summary>
        internal void OnTick()
        {
            if (m_Context.IsDisposed)
                return;
            var user = SyncUser();
            if (IsInitialized && user != null && !SkipServer)
                m_Refresher.RefreshAfterExpiry();
        }
    }
}
