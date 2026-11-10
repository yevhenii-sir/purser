namespace Purser
{
    /// <summary>
    /// Creates the store adapter for <see cref="BillingBootstrap"/> (a component implementing it), e.g. a
    /// <see cref="SteamStoreAdapter"/> with the game's Steamworks bridge. Return null to use the default
    /// (<see cref="UnityIapStoreAdapter"/>).
    /// </summary>
    public interface IStoreAdapterFactory
    {
        IStoreAdapter CreateStore(BillingConfig config);
    }

    /// <summary>The package itself.</summary>
    public static class PurserInfo
    {
        /// <summary>Version of the package (<c>package.json</c>), sent to the server as the SDK version.</summary>
        public const string Version = "0.2.0";
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

        public static IBillingService Create(BillingConfig config, BillingDependencies dependencies)
        {
            return new BillingService(config, dependencies);
        }
    }
}
