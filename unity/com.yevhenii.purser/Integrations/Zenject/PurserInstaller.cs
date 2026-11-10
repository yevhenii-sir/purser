using System;
using UnityEngine;
using Zenject;

namespace Purser.Zenject
{
    /// <summary>
    /// Zenject / Extenject installer. Compiled only with the scripting define <c>PURSER_ZENJECT</c>
    /// (Extenject is usually installed from the Asset Store, so it cannot be detected automatically).
    /// Optional <see cref="IUserIdProvider"/>, <see cref="IAnalyticsIdProvider"/>, <see cref="IAttributionProvider"/> and
    /// <see cref="IStoreAdapter"/> (Amazon, Steam, Stripe, Xsolla) bindings are picked up.
    /// </summary>
    public sealed class PurserInstaller : MonoInstaller
    {
        [SerializeField] private BillingConfig config;
        [SerializeField] private bool initializeOnStart = true;

        public override void InstallBindings()
        {
            Install(Container, config, initializeOnStart);
        }

        public static void Install(DiContainer container, BillingConfig config, bool initializeOnStart = true)
        {
            if (config == null)
                throw new ArgumentNullException(nameof(config));

            container.BindInstance(config).IfNotBound();
            container.Bind<IBillingService>().FromMethod(ctx => new BillingService(config, new BillingDependencies
            {
                UserIdProvider = ctx.Container.TryResolve<IUserIdProvider>(),
                AnalyticsIdProvider = ctx.Container.TryResolve<IAnalyticsIdProvider>(),
                AttributionProvider = ctx.Container.TryResolve<IAttributionProvider>(),
                Store = ctx.Container.TryResolve<IStoreAdapter>(),
            })).AsSingle();

            if (initializeOnStart)
                container.BindInterfacesTo<PurserInitializer>().AsSingle().NonLazy();
        }
    }

    internal sealed class PurserInitializer : IInitializable, IDisposable
    {
        private readonly IBillingService m_Billing;

        public PurserInitializer(IBillingService billing)
        {
            m_Billing = billing;
        }

        public async void Initialize()
        {
            try
            {
                await m_Billing.InitializeAsync();
            }
            catch (Exception e)
            {
                Debug.LogError("[Purser] Initialization failed: " + e.Message);
            }
        }

        public void Dispose() => m_Billing.Dispose();
    }
}
