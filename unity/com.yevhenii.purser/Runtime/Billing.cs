namespace Purser
{
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
