using System;
using System.Threading.Tasks;
using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Creates the store adapter for <see cref="BillingBootstrap"/> (a component implementing it), e.g. a
    /// <see cref="SteamStoreAdapter"/> with the game's Steamworks bridge. Return null to use the default
    /// (<see cref="UnityIapStoreAdapter"/>).
    /// </summary>
    public interface IStoreAdapterFactory
    {
        /// <summary>The store adapter for this config, or null for the default.</summary>
        IStoreAdapter CreateStore(BillingConfig config);
    }

    /// <summary>The package itself.</summary>
    public static class PurserInfo
    {
        /// <summary>Version of the package (<c>package.json</c>), sent to the server as the SDK version.</summary>
        public const string Version = "0.5.0";
    }

    /// <summary>Entry point for code-first setup and DI containers.</summary>
    public static class Billing
    {
        /// <summary>
        /// Creates a billing service. Call <see cref="IBillingService.InitializeAsync"/> once the app starts
        /// (it is fine before the player logs in) and <see cref="IBillingService.SetUserId"/> or pass a
        /// <paramref name="userIdProvider"/> so purchases can be credited.
        /// </summary>
        public static IBillingService Create(BillingConfig config, IUserIdProvider userIdProvider = null,
            IAnalyticsIdProvider analyticsIdProvider = null, IAttributionProvider attributionProvider = null)
        {
            return new BillingService(config, new BillingDependencies
            {
                UserIdProvider = userIdProvider,
                AnalyticsIdProvider = analyticsIdProvider,
                AttributionProvider = attributionProvider,
            });
        }

        /// <summary>Creates a billing service with custom dependencies (store adapter, API client, storage, log sink).</summary>
        public static IBillingService Create(BillingConfig config, BillingDependencies dependencies)
        {
            return new BillingService(config, dependencies);
        }

        /// <summary>
        /// Starts <see cref="IBillingService.InitializeAsync"/> for callers that do not await it (<c>Awake</c>, DI entry
        /// points) and logs a failure to the Unity console. Returns the initialization task itself.
        /// </summary>
        public static Task InitializeInBackground(IBillingService service)
        {
            if (service == null)
                throw new ArgumentNullException(nameof(service));
            var task = service.InitializeAsync();
            LogFailure(task);
            return task;
        }

        private static async void LogFailure(Task initialization)
        {
            try
            {
                await initialization;
            }
            catch (OperationCanceledException)
            {
            }
            catch (Exception e)
            {
                Debug.LogError("[Purser] Initialization failed: " + e.Message);
            }
        }
    }
}
