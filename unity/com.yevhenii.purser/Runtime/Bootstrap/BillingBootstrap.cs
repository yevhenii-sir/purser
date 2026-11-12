using System;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Drop-in component: put it on a GameObject in the first scene, assign a <see cref="BillingConfig"/>,
    /// then use <see cref="Service"/> (or <see cref="Current"/>) and call <see cref="SetUserId"/> after login.
    /// For DI containers use <see cref="Billing.Create(BillingConfig, IUserIdProvider, IAnalyticsIdProvider, IAttributionProvider)"/> instead.
    /// </summary>
    [DefaultExecutionOrder(-1000)]
    [DisallowMultipleComponent]
    [AddComponentMenu("Purser/Billing Bootstrap")]
    public sealed class BillingBootstrap : MonoBehaviour
    {
        [SerializeField] private BillingConfig config;

        [Tooltip("Connect to the store in Awake. Unfinished purchases from the previous session are " +
                 "verified as soon as a user id is set.")]
        [SerializeField] private bool initializeOnAwake = true;

        [SerializeField] private bool dontDestroyOnLoad = true;

        [Tooltip("Optional component implementing IAttributionProvider (e.g. returns AppsFlyer.getAppsFlyerId()).")]
        [SerializeField] private MonoBehaviour attributionProvider;

        [Tooltip("Optional component implementing IAnalyticsIdProvider (Firebase app_instance_id for GA4).")]
        [SerializeField] private MonoBehaviour analyticsIdProvider;

        [Tooltip("Optional component implementing IStoreAdapterFactory: the store adapter for Amazon, Steam, Stripe or " +
                 "Xsolla builds. Empty: Unity IAP (Google Play, App Store).")]
        [SerializeField] private MonoBehaviour storeAdapterFactory;

        /// <summary>The first active bootstrap (convenience for projects without DI).</summary>
        public static BillingBootstrap Current { get; private set; }

        /// <summary>The billing service; null when the bootstrap has no config or is a duplicate.</summary>
        public IBillingService Service { get; private set; }

        /// <summary>Completes when the store is connected and products are loaded.</summary>
        public Task InitializationTask { get; private set; } = Task.CompletedTask;

        /// <summary>The assigned config.</summary>
        public BillingConfig Config => config;

        /// <summary>Sets the player on <see cref="Service"/>.</summary>
        public void SetUserId(string userId) => Service?.SetUserId(userId);

        private void Awake()
        {
            if (Current != null && Current != this)
            {
                Destroy(gameObject);
                return;
            }
            if (config == null)
            {
                Debug.LogError("[Purser] BillingBootstrap has no BillingConfig assigned.", this);
                enabled = false;
                return;
            }

            Current = this;
            if (dontDestroyOnLoad)
                DontDestroyOnLoad(gameObject);

            Service = Billing.Create(config, new BillingDependencies
            {
                AnalyticsIdProvider = analyticsIdProvider as IAnalyticsIdProvider,
                AttributionProvider = attributionProvider as IAttributionProvider,
                Store = (storeAdapterFactory as IStoreAdapterFactory)?.CreateStore(config),
            });
            if (initializeOnAwake)
                InitializationTask = InitializeAsync();
        }

        /// <summary>Initializes <see cref="Service"/> (done in <c>Awake</c> unless "Initialize On Awake" is off); a
        /// failure is logged.</summary>
        public Task InitializeAsync() => Billing.InitializeInBackground(Service);

        private void OnDestroy()
        {
            if (Current != this)
                return;
            Current = null;
            Service?.Dispose();
            Service = null;
        }
    }
}
