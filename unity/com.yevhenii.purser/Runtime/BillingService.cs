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
        // A facade split by topic into partial files (BillingService.*.cs). Orders are verified by OrderProcessor and
        // the customer info is kept by CustomerInfoRefresher; they get the service's collaborators, its lifetime and
        // the current player through BillingContext and call back through IOrderProcessorHost / ICustomerInfoHost.

        /// <summary>Rate-limited verification retries (429 with <c>Retry-After</c>) that do not use up
        /// <see cref="BillingConfig.retryDelaysSeconds"/>.</summary>
        internal const int MaxRateLimitedRetries = OrderProcessor.MaxRateLimitedRetries;

        private readonly BillingConfig m_Config;
        private readonly IStoreAdapter m_Store;
        private readonly IBillingApi m_Api;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly Func<DateTime> m_UtcNow;
        private readonly Func<TimeSpan, CancellationToken, Task> m_Delay;
        private readonly bool m_IsEditor;
        private readonly bool m_CreateLifecycleHook;
        /// <summary>The device, read once on the main thread (Unity APIs).</summary>
        private readonly ClientInfoDto m_Client;

        /// <summary>The collaborators above, the lifetime of the service (disposed or not) and the user id last read
        /// from <see cref="m_Users"/>, shared with <see cref="m_Orders"/> and <see cref="m_Refresher"/>.</summary>
        private readonly BillingContext m_Context;
        /// <summary>Verifies the orders the store delivers and finishes them in the store.</summary>
        private readonly OrderProcessor m_Orders;
        /// <summary>Holds and refreshes the customer info.</summary>
        private readonly CustomerInfoRefresher m_Refresher;

        private readonly Dictionary<string, TaskCompletionSource<PurchaseResult>> m_Waiters =
            new Dictionary<string, TaskCompletionSource<PurchaseResult>>();

        private IUserIdProvider m_Users;
        private Task m_InitTask;
        private List<StoreProduct> m_Products = new List<StoreProduct>();
        private string m_BoundUser;
        private bool m_EverInitialized;
        private bool m_Reconnecting;
        private BillingLifecycleHook m_Hook;

        /// <inheritdoc/>
        public bool IsInitialized { get; private set; }
        /// <inheritdoc/>
        public BillingPlatform Platform { get; }
        /// <inheritdoc/>
        public string Store => m_Store.Store;
        /// <inheritdoc/>
        public IReadOnlyList<StoreProduct> Products => m_Products;
        /// <inheritdoc/>
        public CustomerInfo CustomerInfo => m_Refresher.Current;

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

        private string AppId => m_Context.AppId;

        private bool SkipServer => m_Context.SkipServer;

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
            var attribution = deps.AttributionProvider;
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
            if (config.collectAppleAdsAttribution && Platform == BillingPlatform.Apple && !(attribution is AppleAdsAttributionProvider))
                attribution = new AppleAdsAttributionProvider(attribution, m_Storage, log: m_Log);
            if (deps.PlayerTokenProvider != null)
            {
                // One cache for the API client and the adapter: the game backend is asked once per player.
                var tokens = new PlayerTokenCache(deps.PlayerTokenProvider);
                (m_Api as IPlayerTokenConsumer)?.SetPlayerTokenProvider(tokens);
                (m_Store as IPlayerTokenConsumer)?.SetPlayerTokenProvider(tokens);
            }

            m_Context = new BillingContext(m_Config, m_Store, m_Api, m_Storage, m_Log, m_UtcNow, m_Delay, m_IsEditor, m_Client,
                deps.AnalyticsIdProvider, attribution);
            m_Refresher = new CustomerInfoRefresher(m_Context, this);
            m_Orders = new OrderProcessor(m_Context, this, m_Refresher);

            m_Store.OrderPending += m_Orders.OnOrderPending;
            m_Store.OrderDeferred += OnOrderDeferred;
            m_Store.PurchaseFailed += OnStorePurchaseFailed;
            m_Store.OrderFinished += m_Orders.OnOrderFinished;
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

        private static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;

        private void ThrowIfDisposed()
        {
            if (m_Context.IsDisposed)
                throw new ObjectDisposedException(nameof(BillingService));
        }

        /// <inheritdoc/>
        public void Dispose()
        {
            if (m_Context.IsDisposed)
                return;
            m_Context.Dispose();
            m_Hook?.Release();
            m_Hook = null;
            m_Store.OrderPending -= m_Orders.OnOrderPending;
            m_Store.OrderDeferred -= OnOrderDeferred;
            m_Store.PurchaseFailed -= OnStorePurchaseFailed;
            m_Store.OrderFinished -= m_Orders.OnOrderFinished;
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
