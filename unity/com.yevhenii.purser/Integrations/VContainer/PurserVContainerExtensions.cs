using System;
using VContainer;
using VContainer.Unity;

namespace Purser.VContainer
{
    /// <summary>
    /// VContainer registration. Compiled only when the <c>jp.hadashikick.vcontainer</c> package is installed.
    /// <code>builder.RegisterPurser(billingConfig);</code>
    /// Optional <see cref="IUserIdProvider"/>, <see cref="IAnalyticsIdProvider"/> and <see cref="IAttributionProvider"/> registrations are picked up.
    /// </summary>
    public static class PurserVContainerExtensions
    {
        public static void RegisterPurser(this IContainerBuilder builder, BillingConfig config,
            bool initializeOnStart = true, Lifetime lifetime = Lifetime.Singleton)
        {
            if (config == null)
                throw new ArgumentNullException(nameof(config));

            builder.RegisterInstance(config);
            builder.Register<IBillingService>(resolver => new BillingService(config, new BillingDependencies
            {
                UserIdProvider = resolver.TryResolve<IUserIdProvider>(out var users) ? users : null,
                AnalyticsIdProvider = resolver.TryResolve<IAnalyticsIdProvider>(out var analytics) ? analytics : null,
                AttributionProvider = resolver.TryResolve<IAttributionProvider>(out var attribution) ? attribution : null,
            }), lifetime);

            if (initializeOnStart)
                builder.RegisterEntryPoint<PurserEntryPoint>();
        }
    }

    internal sealed class PurserEntryPoint : IStartable
    {
        private readonly IBillingService m_Billing;

        public PurserEntryPoint(IBillingService billing)
        {
            m_Billing = billing;
        }

        public async void Start()
        {
            try
            {
                await m_Billing.InitializeAsync();
            }
            catch (Exception e)
            {
                UnityEngine.Debug.LogError("[Purser] Initialization failed: " + e.Message);
            }
        }
    }
}
