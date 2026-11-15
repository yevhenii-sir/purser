using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser
{
    /// <summary>Optional dependencies of <see cref="BillingService"/>; anything left null gets a default.</summary>
    public sealed class BillingDependencies
    {
        /// <summary>Default: <see cref="StaticUserIdProvider"/> without a user (call <see cref="IBillingService.SetUserId"/>).
        /// The id is read again before server requests, when the app returns to the foreground and once a second, so a
        /// provider whose id changes (<see cref="FuncUserIdProvider"/>) works like <see cref="IBillingService.SetUserId"/>.</summary>
        public IUserIdProvider UserIdProvider;

        /// <summary>Default: <see cref="UnityIapStoreAdapter"/> (Google Play, App Store). Other stores:
        /// <see cref="AmazonStoreAdapter"/>, <see cref="SteamStoreAdapter"/>, <see cref="StripeStoreAdapter"/>,
        /// <see cref="XsollaStoreAdapter"/> or your own.</summary>
        public IStoreAdapter Store;

        /// <summary>Default: <see cref="BillingApiClient"/> for <see cref="BillingConfig.serverBaseUrl"/> (with the config's
        /// <see cref="BillingConfig.responseSigningPublicKey"/>).</summary>
        public IBillingApi Api;

        /// <summary>Firebase <c>app_instance_id</c> for GA4 events of the server.</summary>
        public IAnalyticsIdProvider AnalyticsIdProvider;

        /// <summary>Device ids for attribution integrations (AppsFlyer, ...).</summary>
        public IAttributionProvider AttributionProvider;

        /// <summary>Player tokens from the game backend for builds that require signed client requests. Given to the API
        /// client and the store adapter when they implement <see cref="IPlayerTokenConsumer"/>.</summary>
        public IPlayerTokenProvider PlayerTokenProvider;

        /// <summary>Default: <see cref="PlayerPrefsBillingStorage"/>.</summary>
        public IBillingStorage Storage;

        /// <summary>Custom log output; default is <c>Debug.Log*</c>.</summary>
        public Action<BillingLogLevel, string> LogSink;

        // Test hooks.
        internal Func<DateTime> UtcNow;
        internal Func<TimeSpan, CancellationToken, Task> Delay;
        internal bool? IsEditor;
        /// <summary>Default: in play mode (foreground refresh, user id checks).</summary>
        internal bool? CreateLifecycleHook;
    }

    /// <summary>
    /// Purchase flow: store purchase → server verification (<c>/verify/{store}</c>) → store confirmation.
    /// The server grants the reward to the game and consumes/acknowledges Google purchases itself,
    /// so the client confirms the order (Apple finishTransaction, Amazon notifyFulfillment) only after
    /// the server accepted or definitively rejected it (<see cref="IStoreAdapter.Finish"/>).
    /// Must be used from the Unity main thread.
    /// </summary>
    public sealed partial class BillingService : IBillingService
    {
        private enum Outcome
        {
            Success,
            PendingDelivery,
            PendingPayment,
            Rejected,
            Retry,
        }

        /// <summary>What the game said about a purchase it started, sent with its <c>/verify</c>.</summary>
        private sealed class PurchaseContext
        {
            public Paywall Paywall;
            public string OfferId;
        }

        /// <summary>Returning to the foreground refreshes the customer info at most this often.</summary>
        internal static readonly TimeSpan ResumeRefreshInterval = TimeSpan.FromMinutes(5);

        /// <summary>Rate-limited verification retries (429 with <c>Retry-After</c>) that do not use up
        /// <see cref="BillingConfig.retryDelaysSeconds"/>.</summary>
        internal const int MaxRateLimitedRetries = 10;

        private readonly BillingConfig m_Config;
        private readonly IStoreAdapter m_Store;
        private readonly IBillingApi m_Api;
        private readonly IAnalyticsIdProvider m_Analytics;
        private readonly IAttributionProvider m_Attribution;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly Func<DateTime> m_UtcNow;
        private readonly Func<TimeSpan, CancellationToken, Task> m_Delay;
        private readonly bool m_IsEditor;
        private readonly bool m_CreateLifecycleHook;
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        /// <summary>The device, read once on the main thread (Unity APIs).</summary>
        private readonly ClientInfoDto m_Client;

        private readonly Dictionary<string, TaskCompletionSource<PurchaseResult>> m_Waiters =
            new Dictionary<string, TaskCompletionSource<PurchaseResult>>();
        /// <summary>Keys (<see cref="StoreOrder.Key"/>) of orders being verified.</summary>
        private readonly HashSet<string> m_InFlightKeys = new HashSet<string>();
        /// <summary>Transaction ids of orders being verified: the same purchase may arrive under another key.</summary>
        private readonly HashSet<string> m_InFlightTransactions = new HashSet<string>();
        private readonly Dictionary<string, StoreOrder> m_WaitingForUser = new Dictionary<string, StoreOrder>();
        /// <summary>Paywall and offer of the purchase started for a store product id (sent with /verify). Kept until the
        /// purchase ends, also when the caller stopped waiting for it.</summary>
        private readonly Dictionary<string, PurchaseContext> m_PurchaseContext = new Dictionary<string, PurchaseContext>();

        private IUserIdProvider m_Users;
        /// <summary>The user id last read from <see cref="m_Users"/>.</summary>
        private string m_KnownUser;
        private Task m_InitTask;
        private List<StoreProduct> m_Products = new List<StoreProduct>();
        private CustomerInfo m_CustomerInfo = CustomerInfo.Empty;
        private DateTime? m_LastRefreshUtc;
        private string m_BoundUser;
        private bool m_EverInitialized;
        private bool m_Reconnecting;
        private BillingLifecycleHook m_Hook;
        private bool m_Disposed;

        /// <inheritdoc/>
        public bool IsInitialized { get; private set; }
        /// <inheritdoc/>
        public BillingPlatform Platform { get; }
        /// <inheritdoc/>
        public string Store => m_Store.Store;
        /// <inheritdoc/>
        public IReadOnlyList<StoreProduct> Products => m_Products;
        /// <inheritdoc/>
        public CustomerInfo CustomerInfo => m_CustomerInfo;

        /// <inheritdoc/>
        public event Action Initialized;
        /// <inheritdoc/>
        public event Action<PurchaseResult> PurchaseSucceeded;
        /// <inheritdoc/>
        public event Action<PurchaseResult> PurchasePending;
        /// <inheritdoc/>
        public event Action<PurchaseResult> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<CustomerInfo> CustomerInfoChanged;
        /// <inheritdoc/>
        public event Action<string> StoreDisconnected;
        /// <inheritdoc/>
        public event Action StoreReconnected;

        private string AppId => m_Config.AppIdFor(m_Store.Store);

        private bool SkipServer => m_IsEditor && m_Config.editorMode == EditorVerificationMode.SkipServer;

        /// <summary>Creates the service; nothing is loaded until <see cref="InitializeAsync"/>.</summary>
        /// <exception cref="BillingException">The store adapter names no store, or an Amazon build has no Amazon adapter.</exception>
        public BillingService(BillingConfig config, BillingDependencies dependencies = null)
        {
            m_Config = config ? config : throw new ArgumentNullException(nameof(config));
            var deps = dependencies ?? new BillingDependencies();
            m_Log = new BillingLog(config.logLevel, deps.LogSink);
            m_Store = deps.Store ?? CreateDefaultStore(config, m_Log);
            Platform = m_Store.Platform;
            if (string.IsNullOrEmpty(m_Store.Store))
                throw new BillingException($"The store adapter {m_Store.GetType().Name} names no store (IStoreAdapter.Store)");
            m_Api = deps.Api ?? new BillingApiClient(config);
            m_Users = deps.UserIdProvider ?? new StaticUserIdProvider();
            m_Analytics = deps.AnalyticsIdProvider;
            m_Attribution = deps.AttributionProvider;
            m_Storage = deps.Storage ?? new PlayerPrefsBillingStorage();
            m_UtcNow = deps.UtcNow ?? (() => DateTime.UtcNow);
            m_Delay = deps.Delay ?? ((delay, ct) => Task.Delay(delay, ct));
            m_IsEditor = deps.IsEditor ?? Application.isEditor;
            m_CreateLifecycleHook = deps.CreateLifecycleHook ?? Application.isPlaying;
            m_Client = new ClientInfoDto
            {
                Device = SystemInfo.deviceModel,
                Os = SystemInfo.operatingSystem,
                AppVersion = Application.version,
                SdkVersion = PurserInfo.Version,
            };
            if (config.collectAppleAdsAttribution && Platform == BillingPlatform.Apple && !(m_Attribution is AppleAdsAttributionProvider))
                m_Attribution = new AppleAdsAttributionProvider(m_Attribution, m_Storage, log: m_Log);
            if (deps.PlayerTokenProvider != null)
            {
                // One cache for the API client and the adapter: the game backend is asked once per player.
                var tokens = new PlayerTokenCache(deps.PlayerTokenProvider);
                (m_Api as IPlayerTokenConsumer)?.SetPlayerTokenProvider(tokens);
                (m_Store as IPlayerTokenConsumer)?.SetPlayerTokenProvider(tokens);
            }

            m_Store.OrderPending += OnOrderPending;
            m_Store.OrderDeferred += OnOrderDeferred;
            m_Store.PurchaseFailed += OnStorePurchaseFailed;
            m_Store.OrderFinished += OnOrderFinished;
            if (m_Store is IStoreConnectionEvents connection)
                connection.Disconnected += OnStoreDisconnected;
            if (m_Store is ISubscriptionChangeStore changes)
                changes.ChangeDeferred += OnSubscriptionChangeDeferred;
        }

        private static IStoreAdapter CreateDefaultStore(BillingConfig config, BillingLog log)
        {
            var platform = BillingPlatforms.For(config);
            if (platform == BillingPlatform.Amazon)
            {
                if (config.androidStore == AndroidStore.AmazonAppstore)
                    throw new BillingException("androidStore is Amazon Appstore: pass an AmazonStoreAdapter in BillingDependencies.Store " +
                                               "(Unity IAP 5 has no Amazon store)");
                log.Warning("Installed by the Amazon Appstore, but no AmazonStoreAdapter was given in BillingDependencies.Store: using Google Play.");
                platform = BillingPlatform.Google;
            }
            return new UnityIapStoreAdapter(platform, log);
        }

        // ------------------------------------------------------------------
        // Initialization & user
        // ------------------------------------------------------------------

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
            LoadCachedCustomerInfo(m_KnownUser);
            if (m_Attribution is AppleAdsAttributionProvider appleAds)
                LogFailure(appleAds.WarmUpAsync(), "Apple Ads attribution");
            IsInitialized = true;
            m_EverInitialized = true;
            if (m_CreateLifecycleHook && m_Hook == null)
                m_Hook = BillingLifecycleHook.Create(this);
            Initialized?.Invoke();

            ProcessWaitingOrders();
            if (m_Config.refreshEntitlementsOnInit && m_KnownUser != null)
                RefreshInBackground();
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

        private const string RefundConsentKey = "purser.refund_data_consent";

        /// <inheritdoc/>
        public void SetRefundDataConsent(bool consented)
        {
            m_Storage.Save(RefundConsentKey, consented ? "true" : "false");
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
            if (current == m_KnownUser)
                return current;
            m_KnownUser = current;
            m_Log.Info(current == null ? "The player signed out." : "Player " + current);
            LoadCachedCustomerInfo(current);
            BindAccount();
            ProcessWaitingOrders();
            if (refreshOnChange && IsInitialized && current != null)
                RefreshInBackground();
            return current;
        }

        private void BindAccount()
        {
            var user = m_KnownUser;
            if (!m_Config.bindPurchasesToUser || user == null || user == m_BoundUser)
                return;
            m_Store.SetAccountBinding(user);
            m_BoundUser = user;
        }

        /// <summary>The app came back to the foreground (<see cref="BillingLifecycleHook"/>): notices a changed user id
        /// and refreshes the customer info, at most once per <see cref="ResumeRefreshInterval"/>.</summary>
        internal void OnAppResumed()
        {
            if (m_Disposed)
                return;
            var user = SyncUser();
            if (!IsInitialized || user == null || SkipServer)
                return;
            var now = m_UtcNow();
            if (m_LastRefreshUtc is DateTime last && now >= last && now - last < ResumeRefreshInterval)
                return;
            RefreshInBackground();
        }

        /// <summary>Once a second in play mode: notices a changed user id of the provider.</summary>
        internal void OnTick()
        {
            if (!m_Disposed)
                SyncUser();
        }

        // ------------------------------------------------------------------
        // Store connection
        // ------------------------------------------------------------------

        private void OnStoreDisconnected(string reason)
        {
            if (m_Disposed || !m_EverInitialized || m_Reconnecting)
                return;
            IsInitialized = false;
            m_Log.Warning($"Store disconnected ({reason}); reconnecting.");
            try
            {
                StoreDisconnected?.Invoke(reason);
            }
            catch (Exception e)
            {
                m_Log.Error($"{nameof(StoreDisconnected)} handler threw: {e}");
            }
            _ = ReconnectAsync();
        }

        /// <summary>Delay before reconnection attempt <paramref name="attempt"/>: 5 s, doubling, at most 5 minutes.</summary>
        internal static TimeSpan ReconnectDelay(int attempt) =>
            TimeSpan.FromSeconds(Math.Min(300, 5 * Math.Pow(2, Math.Min(attempt, 6))));

        private async Task ReconnectAsync()
        {
            m_Reconnecting = true;
            try
            {
                for (var attempt = 0; !m_Disposed; attempt++)
                {
                    await m_Delay(ReconnectDelay(attempt), m_Lifetime.Token);
                    if (m_Disposed)
                        return;
                    try
                    {
                        await m_Store.ConnectAsync(m_Lifetime.Token);
                    }
                    catch (OperationCanceledException) when (m_Disposed)
                    {
                        return;
                    }
                    catch (Exception e)
                    {
                        m_Log.Warning($"Store reconnection attempt {attempt + 1} failed: {e.Message}");
                        continue;
                    }
                    if (m_Disposed)
                        return;
                    IsInitialized = true;
                    m_Log.Info("Store reconnected.");
                    try
                    {
                        StoreReconnected?.Invoke();
                    }
                    catch (Exception e)
                    {
                        m_Log.Error($"{nameof(StoreReconnected)} handler threw: {e}");
                    }
                    ProcessWaitingOrders();
                    if (SyncUser(refreshOnChange: false) != null)
                        RefreshInBackground();
                    return;
                }
            }
            catch (OperationCanceledException)
            {
                // Disposed.
            }
            catch (ObjectDisposedException)
            {
                // Disposed.
            }
            finally
            {
                m_Reconnecting = false;
            }
        }

        // ------------------------------------------------------------------
        // Purchases
        // ------------------------------------------------------------------

        /// <inheritdoc/>
        public StoreProduct GetProduct(string productId) =>
            m_Products.FirstOrDefault(p => p.Id == productId) ?? m_Products.FirstOrDefault(p => p.StoreId == productId);

        /// <inheritdoc/>
        public Task<PurchaseResult> PurchaseAsync(string productId, CancellationToken ct = default) =>
            PurchaseCoreAsync(productId, null, ct);

        /// <inheritdoc/>
        public Task<PurchaseResult> PurchaseAsync(string productId, Paywall paywall, CancellationToken ct = default) =>
            PurchaseCoreAsync(productId, new PurchaseOptions { Paywall = paywall }, ct);

        /// <inheritdoc/>
        public Task<PurchaseResult> PurchaseAsync(string productId, PurchaseOptions options, CancellationToken ct = default) =>
            PurchaseCoreAsync(productId, options, ct);

        private async Task<PurchaseResult> PurchaseCoreAsync(string productId, PurchaseOptions options, CancellationToken ct)
        {
            if (!IsInitialized || m_Disposed)
                return new PurchaseResult(PurchaseStatus.NotInitialized, productId, message: "billing is not initialized");

            var config = m_Config.FindById(productId) ?? m_Config.FindByStoreId(productId);
            if (config == null)
                return new PurchaseResult(PurchaseStatus.Failed, productId, message: "unknown product");
            var storeId = config.StoreId(m_Store.Store);
            if (SyncUser() == null)
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: "user id is not set");
            if (m_Waiters.ContainsKey(storeId))
                return new PurchaseResult(PurchaseStatus.AlreadyInProgress, config.id, storeId);

            string replacedStoreId = null;
            if (!string.IsNullOrEmpty(options?.ReplacesProductId))
            {
                var replaced = m_Config.FindById(options.ReplacesProductId) ?? m_Config.FindByStoreId(options.ReplacesProductId);
                string problem = null;
                if (replaced == null)
                    problem = $"unknown product to replace: {options.ReplacesProductId}";
                else if (config.type != BillingProductType.Subscription || replaced.type != BillingProductType.Subscription)
                    problem = "only a subscription can replace a subscription";
                else if (replaced.StoreId(m_Store.Store) == storeId)
                    problem = "the subscription cannot replace itself";
                else if (!(m_Store is ISubscriptionChangeStore))
                    problem = $"the {m_Store.Store} store adapter does not change subscriptions";
                if (problem != null)
                    return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: problem);
                replacedStoreId = replaced.StoreId(m_Store.Store);
            }

            BindAccount();
            var waiter = new TaskCompletionSource<PurchaseResult>();
            m_Waiters[storeId] = waiter;
            SetContext(storeId, options?.Paywall, options?.OfferId);
            try
            {
                if (m_Store is ICheckoutStoreAdapter checkout)
                    checkout.PrepareCheckout(new CheckoutContext { UserId = m_KnownUser, Attribution = await AttributionAsync() });
                if (m_Disposed)
                    return new PurchaseResult(PurchaseStatus.NotInitialized, config.id, storeId, message: "billing was disposed");
                if (replacedStoreId != null)
                    ((ISubscriptionChangeStore)m_Store).PurchaseReplacing(storeId, replacedStoreId, options.ReplacementMode);
                else
                    m_Store.Purchase(storeId);
            }
            catch (Exception e)
            {
                m_Waiters.Remove(storeId);
                m_PurchaseContext.Remove(storeId);
                return new PurchaseResult(PurchaseStatus.Failed, config.id, storeId, message: e.Message);
            }

            return await AwaitResultAsync(storeId, waiter, ct);
        }

        /// <summary>Waits for the result of the store purchase. Cancelling <paramref name="ct"/> stops waiting only: the
        /// purchase goes on in the store and keeps its paywall and offer for the server.</summary>
        private async Task<PurchaseResult> AwaitResultAsync(string storeId, TaskCompletionSource<PurchaseResult> waiter,
            CancellationToken ct)
        {
            using (ct.Register(() => waiter.TrySetCanceled()))
            {
                try
                {
                    return await waiter.Task;
                }
                finally
                {
                    if (m_Waiters.TryGetValue(storeId, out var current) && current == waiter)
                        m_Waiters.Remove(storeId);
                }
            }
        }

        private void SetContext(string storeId, Paywall paywall, string offerId)
        {
            if (paywall == null && string.IsNullOrEmpty(offerId))
                m_PurchaseContext.Remove(storeId);
            else
                m_PurchaseContext[storeId] = new PurchaseContext { Paywall = paywall, OfferId = NullIfEmpty(offerId) };
        }

        private bool IsInFlight(StoreOrder order) =>
            m_InFlightKeys.Contains(order.Key) ||
            (!string.IsNullOrEmpty(order.TransactionId) && m_InFlightTransactions.Contains(order.TransactionId));

        private void OnOrderPending(StoreOrder order)
        {
            if (m_Disposed)
                return;
            var key = order.Key;
            if (string.IsNullOrEmpty(key))
            {
                m_Log.Error($"Order {order} has no transaction id; skipped.");
                return;
            }
            if (IsInFlight(order))
                return;
            if (SyncUser() == null)
            {
                m_WaitingForUser[key] = order;
                m_Log.Info($"Order {order} waits for a user id.");
                return;
            }
            _ = ProcessOrderAsync(order);
        }

        private void ProcessWaitingOrders()
        {
            if (m_KnownUser == null || m_WaitingForUser.Count == 0)
                return;
            var orders = m_WaitingForUser.Values.ToList();
            m_WaitingForUser.Clear();
            foreach (var order in orders)
                OnOrderPending(order);
        }

        private async Task ProcessOrderAsync(StoreOrder order)
        {
            var key = order.Key;
            var transactionId = string.IsNullOrEmpty(order.TransactionId) ? null : order.TransactionId;
            m_InFlightKeys.Add(key);
            if (transactionId != null)
                m_InFlightTransactions.Add(transactionId);
            var config = m_Config.FindByStoreId(order.StoreProductId);
            var productId = config?.id ?? order.StoreProductId;
            try
            {
                var user = m_KnownUser;
                if (SkipServer)
                {
                    m_Store.Finish(order, true);
                    if (config != null)
                        GrantLocally(user, config);
                    Report(new PurchaseResult(PurchaseStatus.Success, productId, order.StoreProductId,
                        order.TransactionId ?? key, "editor: confirmed locally without the server"));
                    return;
                }

                var request = await BuildVerifyRequestAsync(order, user);
                if (m_Disposed)
                    return;
                var delays = m_Config.retryDelaysSeconds ?? new int[0];
                var attempt = 0;
                var rateLimited = 0;
                var reported = false;
                while (true)
                {
                    // Webhook stores (Stripe, Xsolla) have no receipt: the adapter asks the server about the order.
                    var response = m_Store is IWebhookStoreAdapter webhookStore
                        ? await webhookStore.CheckOrderAsync(order, request, m_Lifetime.Token)
                        : await m_Api.VerifyAsync(m_Store.Store, request, m_Lifetime.Token);
                    if (m_Disposed)
                        return;
                    var outcome = Classify(response);
                    if (outcome != Outcome.Retry)
                    {
                        if (config == null && !string.IsNullOrEmpty(response.Value?.ProductId))
                        {
                            // The store did not name the product (a Steam order authorized before a restart, a resumed
                            // Stripe session): the server's answer does.
                            config = m_Config.FindByStoreId(response.Value.ProductId);
                            productId = config?.id ?? response.Value.ProductId;
                        }
                        Apply(order, config, productId, response.Value, outcome);
                        return;
                    }

                    m_Log.Warning($"Verification of {order} failed ({response}), attempt {attempt + rateLimited + 1}.");
                    if (!reported)
                    {
                        reported = true;
                        Report(new PurchaseResult(PurchaseStatus.PendingVerification, productId, order.StoreProductId,
                            order.TransactionId, "payment received, verification will be retried", response?.Value));
                    }
                    TimeSpan wait;
                    if (response != null && response.IsRateLimited && response.RetryAfter.HasValue && rateLimited < MaxRateLimitedRetries)
                    {
                        // The server asked to wait (429): it is not a failed attempt.
                        rateLimited++;
                        wait = response.RetryAfter.Value;
                    }
                    else
                    {
                        if (attempt >= delays.Length)
                        {
                            m_Log.Warning($"Giving up on {order} for this session; the store will deliver it again on next launch.");
                            return;
                        }
                        wait = TimeSpan.FromSeconds(delays[attempt]);
                        attempt++;
                    }
                    await m_Delay(wait, m_Lifetime.Token);
                    if (m_Disposed)
                        return;
                }
            }
            catch (OperationCanceledException)
            {
                // Service disposed.
            }
            catch (ObjectDisposedException) when (m_Disposed)
            {
            }
            catch (Exception e)
            {
                m_Log.Error($"Processing {order} failed: {e}");
                Report(new PurchaseResult(PurchaseStatus.PendingVerification, productId, order.StoreProductId,
                    order.TransactionId, e.Message));
            }
            finally
            {
                m_InFlightKeys.Remove(key);
                if (transactionId != null)
                    m_InFlightTransactions.Remove(transactionId);
            }
        }

        private async Task<VerifyRequest> BuildVerifyRequestAsync(StoreOrder order, string user)
        {
            // A fallback price is the game's guess, not what the store charged: the server is not told it.
            var product = m_Products.FirstOrDefault(p => p.StoreId == order.StoreProductId && !p.IsFallbackPrice);
            // Read before the first await: the purchase that set it may finish meanwhile.
            m_PurchaseContext.TryGetValue(order.StoreProductId, out var context);
            var paywall = context?.Paywall;
            return new VerifyRequest
            {
                AppId = AppId,
                UserId = user,
                ProductId = order.StoreProductId,
                PurchaseToken = NullIfEmpty(order.PurchaseToken),
                OrderId = string.IsNullOrEmpty(order.TransactionId) ? null : order.TransactionId,
                StoreUserId = string.IsNullOrEmpty(order.StoreUserId) ? null : order.StoreUserId,
                AppInstanceId = await AppInstanceIdAsync(),
                PriceMicros = product?.PriceMicros ?? 0,
                Currency = string.IsNullOrEmpty(product?.Currency) ? null : product.Currency,
                Attribution = await AttributionAsync(),
                Placement = NullIfEmpty(paywall?.PlacementId),
                VariantId = NullIfEmpty(paywall?.VariantId),
                TestId = NullIfEmpty(paywall?.TestId),
                PaywallId = NullIfEmpty(paywall?.PaywallId),
                OfferId = context?.OfferId,
                Client = m_Client,
            };
        }

        private static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;

        private async Task<Dictionary<string, string>> AttributionAsync()
        {
            var clean = new Dictionary<string, string>();
            if (m_Attribution != null)
            {
                try
                {
                    var values = await m_Attribution.GetAttributionAsync();
                    if (values != null)
                        foreach (var kv in values.Where(kv => !string.IsNullOrEmpty(kv.Key) && !string.IsNullOrEmpty(kv.Value)))
                            clean[kv.Key] = kv.Value;
                }
                catch (Exception e)
                {
                    m_Log.Verbose("Attribution ids unavailable: " + e.Message);
                }
            }
            // Always explicit ("false" too): the server keeps the last value it received.
            var consent = m_Storage.Load(RefundConsentKey);
            if (!string.IsNullOrEmpty(consent))
                clean[AttributionKeys.RefundDataConsent] = consent;
            return clean.Count == 0 ? null : clean;
        }

        private async Task<string> AppInstanceIdAsync()
        {
            if (m_Analytics == null)
                return null;
            try
            {
                var id = await m_Analytics.GetAppInstanceIdAsync();
                return string.IsNullOrEmpty(id) ? null : id;
            }
            catch (Exception e)
            {
                m_Log.Verbose("App instance id unavailable: " + e.Message);
                return null;
            }
        }

        private static Outcome Classify(ApiResult<VerifyResponse> response)
        {
            if (response == null || response.IsTransportError || response.IsRateLimited)
                return Outcome.Retry;
            switch (response.Value?.Status)
            {
                case VerifyStatus.Success:
                    return Outcome.Success;
                case VerifyStatus.Pending:
                    return Outcome.PendingDelivery;
                case VerifyStatus.PendingOrCanceled:
                    return Outcome.PendingPayment;
                case VerifyStatus.Invalid:
                case VerifyStatus.Forbidden:
                case VerifyStatus.Expired:
                case VerifyStatus.Revoked:
                    return Outcome.Rejected;
                default:
                    // `error` (server misconfiguration, store outage, refused player token), 5xx, unexpected body: keep
                    // the order and retry.
                    return Outcome.Retry;
            }
        }

        private void Apply(StoreOrder order, BillingProductConfig config, string productId, VerifyResponse server, Outcome outcome)
        {
            var transactionId = server?.TransactionId ?? order.TransactionId;
            switch (outcome)
            {
                case Outcome.Success:
                case Outcome.PendingDelivery:
                    var result = new PurchaseResult(
                        outcome == Outcome.Success ? PurchaseStatus.Success : PurchaseStatus.PendingDelivery,
                        productId, order.StoreProductId, transactionId, server?.Message, server);
                    if (result.GrantOnClient)
                    {
                        // The game grants the reward in PurchaseSucceeded. Finish the purchase (Google
                        // consume / Apple finishTransaction) only after that: if the app dies in between,
                        // the store delivers it again and the server answers success from its ledger.
                        if (!Report(result))
                        {
                            m_Log.Error($"PurchaseSucceeded handler failed for {order}, not finishing it " +
                                        "so the store delivers it again.");
                            return;
                        }
                        m_Store.Finish(order, true);
                    }
                    else
                    {
                        m_Store.Finish(order, true);
                        Report(result);
                    }
                    if (config == null || config.type != BillingProductType.Consumable)
                        RefreshInBackground();
                    break;
                case Outcome.PendingPayment:
                    Report(new PurchaseResult(PurchaseStatus.PendingPayment, productId, order.StoreProductId,
                        transactionId, "payment is not completed yet", server));
                    break;
                case Outcome.Rejected:
                    // Definitive answer: finish the transaction so the store stops re-delivering it.
                    m_Store.Finish(order, false);
                    Report(new PurchaseResult(PurchaseStatus.Rejected, productId, order.StoreProductId,
                        transactionId, $"{server?.Status}: {server?.Message}", server));
                    break;
            }
        }

        private void OnOrderDeferred(StoreOrder order)
        {
            var config = m_Config.FindByStoreId(order.StoreProductId);
            Report(new PurchaseResult(PurchaseStatus.PendingPayment, config?.id ?? order.StoreProductId,
                order.StoreProductId, order.TransactionId, "waiting for approval"));
        }

        private void OnSubscriptionChangeDeferred(string storeProductId)
        {
            if (m_Disposed)
                return;
            var config = m_Config.FindByStoreId(storeProductId);
            m_PurchaseContext.Remove(storeProductId);
            Report(new PurchaseResult(PurchaseStatus.PendingPayment, config?.id ?? storeProductId, storeProductId,
                message: "the subscription change takes effect at the next renewal"));
            RefreshInBackground();
        }

        private void OnStorePurchaseFailed(StorePurchaseFailure failure)
        {
            var status = failure.UserCancelled ? PurchaseStatus.Cancelled : PurchaseStatus.Failed;
            if (string.IsNullOrEmpty(failure.StoreProductId))
            {
                // The store did not say which purchase failed: fail every open one rather than leave them waiting.
                if (m_Waiters.Count == 0)
                    Report(new PurchaseResult(status, null, null, message: failure.Reason));
                foreach (var open in m_Waiters.Keys.ToList())
                    Report(new PurchaseResult(status, m_Config.FindByStoreId(open)?.id ?? open, open, message: failure.Reason));
                return;
            }
            var storeId = failure.StoreProductId;
            Report(new PurchaseResult(status, m_Config.FindByStoreId(storeId)?.id ?? storeId, storeId, message: failure.Reason));
        }

        private void OnOrderFinished(StoreOrder order, bool success, string error)
        {
            if (!success)
                m_Log.Verbose($"Store confirmation of {order} reported: {error} " +
                              "(expected when the server already consumed the purchase).");
        }

        /// <summary>Completes the purchase's waiter and raises its event. False when a game handler threw (logged,
        /// never passed on to the store callbacks).</summary>
        private bool Report(PurchaseResult result)
        {
            if (!string.IsNullOrEmpty(result.StoreProductId))
            {
                if (m_Waiters.TryGetValue(result.StoreProductId, out var waiter))
                {
                    m_Waiters.Remove(result.StoreProductId);
                    waiter.TrySetResult(result);
                }
                // The purchase ended: its paywall and offer are not sent with later orders of the product. A pending
                // one keeps them for the order the store delivers later.
                if (result.Status != PurchaseStatus.PendingPayment && result.Status != PurchaseStatus.PendingVerification)
                    m_PurchaseContext.Remove(result.StoreProductId);
            }

            m_Log.Info("Purchase " + result);
            Action<PurchaseResult> handlers;
            string name;
            switch (result.Status)
            {
                case PurchaseStatus.Success:
                case PurchaseStatus.PendingDelivery:
                    (handlers, name) = (PurchaseSucceeded, nameof(PurchaseSucceeded));
                    break;
                case PurchaseStatus.PendingPayment:
                case PurchaseStatus.PendingVerification:
                    (handlers, name) = (PurchasePending, nameof(PurchasePending));
                    break;
                default:
                    (handlers, name) = (PurchaseFailed, nameof(PurchaseFailed));
                    break;
            }
            try
            {
                handlers?.Invoke(result);
                return true;
            }
            catch (Exception e)
            {
                m_Log.Error($"{name} handler threw for {result}: {e}");
                return false;
            }
        }

        // ------------------------------------------------------------------
        // Customer info & restore
        // ------------------------------------------------------------------

        /// <inheritdoc/>
        public async Task<CustomerInfo> RestoreAsync(CancellationToken ct = default)
        {
            if (!IsInitialized)
                throw new BillingException("Billing is not initialized");
            var user = SyncUser(refreshOnChange: false) ?? throw new BillingException("User id is not set");
            if (SkipServer)
                return m_CustomerInfo;

            if (Platform == BillingPlatform.Apple)
                await m_Store.RestoreTransactionsAsync(ct);

            // Orders being verified right now (e.g. re-delivered by RestoreTransactions) are not sent again.
            var orders = await m_Store.FetchPurchasesAsync(ct);
            var items = orders
                .Where(o => !string.IsNullOrEmpty(o.Key) && !IsInFlight(o))
                .GroupBy(o => o.Key)
                .Select(g => g.First())
                .Select(o => new RestoreItem
                {
                    ProductId = o.StoreProductId,
                    PurchaseToken = NullIfEmpty(o.PurchaseToken),
                    OrderId = string.IsNullOrEmpty(o.TransactionId) ? null : o.TransactionId,
                    StoreUserId = string.IsNullOrEmpty(o.StoreUserId) ? null : o.StoreUserId,
                })
                .ToList();
            if (items.Count == 0)
                return await RefreshCustomerInfoAsync(ct);

            CustomerInfoDto latest = null;
            var appInstanceId = await AppInstanceIdAsync();
            var attribution = await AttributionAsync();
            for (var i = 0; i < items.Count; i += RestoreRequest.MaxItems)
            {
                var request = new RestoreRequest
                {
                    AppId = AppId,
                    UserId = user,
                    AppInstanceId = appInstanceId,
                    Attribution = attribution,
                    Purchases = items.Skip(i).Take(RestoreRequest.MaxItems).ToList(),
                    Client = m_Client,
                };
                var response = await m_Api.RestoreAsync(request, ct);
                if (response.IsHttpSuccess && response.Value?.CustomerInfo != null)
                    latest = response.Value.CustomerInfo;
                else
                    m_Log.Warning("Restore request failed: " + response);
            }

            if (latest == null)
                return await RefreshCustomerInfoAsync(ct);
            SetCustomerInfo(user, latest);
            return m_CustomerInfo;
        }

        /// <inheritdoc/>
        public async Task<CustomerInfo> RefreshCustomerInfoAsync(CancellationToken ct = default)
        {
            var user = SyncUser(refreshOnChange: false);
            if (user == null)
                return CustomerInfo.Empty;
            if (SkipServer)
                return m_CustomerInfo;

            m_LastRefreshUtc = m_UtcNow();
            var response = await m_Api.EntitlementsAsync(new EntitlementsRequest { AppId = AppId, UserId = user, Client = m_Client }, ct);
            if (response.IsHttpSuccess && response.Value != null)
            {
                SetCustomerInfo(user, response.Value);
                return m_CustomerInfo;
            }

            m_Log.Warning("Customer info request failed: " + response);
            return m_CustomerInfo.UserId == user ? m_CustomerInfo.AsCached() : CustomerInfo.Empty;
        }

        private async void RefreshInBackground()
        {
            if (m_Disposed)
                return;
            try
            {
                await RefreshCustomerInfoAsync(m_Lifetime.Token);
            }
            catch (OperationCanceledException)
            {
            }
            catch (Exception e)
            {
                m_Log.Warning("Customer info refresh failed: " + e.Message);
            }
        }

        private void SetCustomerInfo(string user, CustomerInfoDto dto)
        {
            if (user != m_KnownUser)
                return; // the player changed while the request was running
            var now = m_UtcNow();
            CustomerInfoCache.Save(m_Storage, AppId, user, dto, now, m_Log);
            SetCustomerInfo(CustomerInfo.FromDto(user, dto, now, false));
        }

        /// <summary>Keeps <paramref name="info"/> and raises <see cref="CustomerInfoChanged"/> when what it gives
        /// differs from the previous one.</summary>
        private void SetCustomerInfo(CustomerInfo info)
        {
            var changed = !info.HasSameContent(m_CustomerInfo);
            m_CustomerInfo = info;
            if (!changed)
                return;
            try
            {
                CustomerInfoChanged?.Invoke(info);
            }
            catch (Exception e)
            {
                m_Log.Error($"{nameof(CustomerInfoChanged)} handler threw: {e}");
            }
        }

        private void LoadCachedCustomerInfo(string user)
        {
            var cached = user == null ? null : CustomerInfoCache.Load(m_Storage, AppId, user, m_Log);
            SetCustomerInfo(cached ?? CustomerInfo.Empty);
        }

        /// <summary>Editor without the server: grants the product and its access levels, cached like a server answer
        /// so the next Editor session has them too.</summary>
        private void GrantLocally(string user, BillingProductConfig config)
        {
            if (user == null)
                return;
            var info = m_CustomerInfo.WithLocalGrant(user, config, m_UtcNow());
            if (ReferenceEquals(info, m_CustomerInfo))
                return;
            CustomerInfoCache.Save(m_Storage, AppId, user, info.ToDto(), info.FetchedAtUtc, m_Log);
            SetCustomerInfo(info);
        }

        /// <summary>Expiring items of the customer info count (<see cref="BillingConfig.entitlementsMaxAgeDays"/>).</summary>
        private bool IsTrusted(CustomerInfo info, DateTime now)
        {
            var maxAgeDays = m_Config.entitlementsMaxAgeDays;
            return maxAgeDays <= 0 || info.IsFresh(now, TimeSpan.FromDays(maxAgeDays));
        }

        /// <inheritdoc/>
        /// <remarks>Subscriptions count only while the customer info is fresh (<see cref="BillingConfig.entitlementsMaxAgeDays"/>,
        /// <see cref="CustomerInfo.IsFresh"/>); non-consumables always do.</remarks>
        public bool IsActive(string productId)
        {
            var now = m_UtcNow();
            var info = m_CustomerInfo;
            var trusted = IsTrusted(info, now);
            return ProductIds(productId).Any(id => info.Owns(id) || info.IsSubscriptionActive(id, now, trusted));
        }

        /// <inheritdoc/>
        public bool HasAccess(string accessLevel)
        {
            if (string.IsNullOrEmpty(accessLevel))
                return false;
            var now = m_UtcNow();
            var info = m_CustomerInfo;
            return info.HasAccess(accessLevel, now, IsTrusted(info, now));
        }

        /// <inheritdoc/>
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

        // ------------------------------------------------------------------
        // Paywalls & A/B tests
        // ------------------------------------------------------------------

        /// <inheritdoc/>
        public async Task<Paywall> GetPaywallAsync(string placementId, CancellationToken ct = default)
        {
            ThrowIfDisposed();
            if (string.IsNullOrEmpty(placementId))
                throw new ArgumentException("Placement id is empty", nameof(placementId));
            var user = SyncUser();
            var response = await m_Api.PaywallAsync(new PaywallRequest { AppId = AppId, Placement = placementId, UserId = user, Client = m_Client }, ct);
            if (response.IsHttpSuccess && response.Value != null)
            {
                var now = m_UtcNow();
                PaywallCache.Save(m_Storage, AppId, user, placementId, response.Value, now, m_Log);
                return ToPaywall(placementId, response.Value, false, now);
            }

            if (response.HttpStatus == 404)
            {
                // The placement was removed or disabled on purpose: forget the cached answer.
                PaywallCache.Save(m_Storage, AppId, user, placementId, null, m_UtcNow(), m_Log);
                throw new PaywallNotFoundException(placementId, response.Value?.Code ?? "ERR_PLACEMENT_NOT_FOUND",
                    response.Value?.Message ?? $"no placement '{placementId}'");
            }
            if (response.HttpStatus >= 400 && response.HttpStatus < 500 && !response.IsRateLimited)
                throw new BillingException($"Paywall request for '{placementId}' was refused: {response.Value?.Message ?? response.Error}");

            var cached = PaywallCache.Load(m_Storage, AppId, user, placementId, m_Log);
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
            if (paywall == null || string.IsNullOrEmpty(paywall.VariantId) || m_Disposed)
                return false;
            if (SkipServer)
                return false; // Editor sessions would skew the A/B test views.
            try
            {
                var response = await m_Api.PaywallEventAsync(new PaywallEventRequest
                {
                    AppId = AppId,
                    Placement = paywall.PlacementId,
                    VariantId = paywall.VariantId,
                    TestId = NullIfEmpty(paywall.TestId),
                    UserId = SyncUser(),
                    Event = PaywallEventRequest.View,
                }, ct);
                if (!response.IsHttpSuccess)
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

        // ------------------------------------------------------------------

        private void ThrowIfDisposed()
        {
            if (m_Disposed)
                throw new ObjectDisposedException(nameof(BillingService));
        }

        /// <inheritdoc/>
        public void Dispose()
        {
            if (m_Disposed)
                return;
            m_Disposed = true;
            m_Lifetime.Cancel();
            m_Lifetime.Dispose();
            m_Hook?.Release();
            m_Hook = null;
            m_Store.OrderPending -= OnOrderPending;
            m_Store.OrderDeferred -= OnOrderDeferred;
            m_Store.PurchaseFailed -= OnStorePurchaseFailed;
            m_Store.OrderFinished -= OnOrderFinished;
            if (m_Store is IStoreConnectionEvents connection)
                connection.Disconnected -= OnStoreDisconnected;
            if (m_Store is ISubscriptionChangeStore changes)
                changes.ChangeDeferred -= OnSubscriptionChangeDeferred;
            m_Store.Dispose();
            foreach (var waiter in m_Waiters.Values.ToList())
                waiter.TrySetCanceled();
            m_Waiters.Clear();
        }
    }
}
