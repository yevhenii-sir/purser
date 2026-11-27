using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Makes <see cref="UnityIapStoreAdapter"/> the default store of <see cref="BillingService"/>
    /// (<c>Billing.DefaultStoreFactory</c>). This assembly is compiled only with Unity IAP 5.4 or later
    /// (<c>PURSER_UNITY_IAP5</c>), so the core works without Unity IAP.
    /// </summary>
    // Kept from managed code stripping (also in link.xml): no code calls it, the player does on load.
    [UnityEngine.Scripting.Preserve]
    internal static class UnityIapRegistration
    {
        [UnityEngine.Scripting.Preserve]
        [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.SubsystemRegistration)]
        private static void RegisterAtRuntime() => Register();

#if UNITY_EDITOR
        [UnityEditor.InitializeOnLoadMethod]
        private static void RegisterInEditor() => Register();
#endif

        internal static void Register()
        {
            Billing.DefaultStoreFactory = (platform, log) => new UnityIapStoreAdapter(platform, log);
        }
    }
}
