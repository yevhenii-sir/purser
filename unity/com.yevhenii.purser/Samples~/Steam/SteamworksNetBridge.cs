// ISteamMicroTxnBridge over Steamworks.NET (https://github.com/rlabrecque/Steamworks.NET).
// 1. Install Steamworks.NET and initialize the Steam API as usual (e.g. its SteamManager component).
// 2. Add PURSER_STEAMWORKS_NET to Player Settings > Scripting Define Symbols (Standalone).
// 3. Pass the adapter to the billing service on Steam builds:
//      var bridge = new SteamworksNetBridge(() => SteamManager.Initialized);
//      var deps = new BillingDependencies { UserIdProvider = ..., Store = new SteamStoreAdapter(config, bridge) };
//      var billing = Billing.Create(config, deps);
// Steamworks.NET raises callbacks from SteamAPI.RunCallbacks (SteamManager calls it in Update), so
// AuthorizationReceived arrives on the main thread.
#if PURSER_STEAMWORKS_NET
using System;
using Steamworks;

namespace Purser.Samples
{
    public sealed class SteamworksNetBridge : ISteamMicroTxnBridge, IDisposable
    {
        private readonly Func<bool> m_Initialized;
        // Kept in a field: a collected Callback stops delivering.
        private readonly Callback<MicroTxnAuthorizationResponse_t> m_Authorization;

        public event Action<uint, ulong, bool> AuthorizationReceived;

        /// <param name="initialized">Whether the game initialized the Steam API (e.g. <c>() => SteamManager.Initialized</c>).
        /// Steamworks calls before <c>SteamAPI.Init</c> crash, so the bridge asks first.</param>
        public SteamworksNetBridge(Func<bool> initialized)
        {
            m_Initialized = initialized ?? throw new ArgumentNullException(nameof(initialized));
            m_Authorization = Callback<MicroTxnAuthorizationResponse_t>.Create(OnAuthorization);
        }

        public bool IsReady => m_Initialized() && SteamUser.BLoggedOn();

        public ulong SteamId => m_Initialized() ? SteamUser.GetSteamID().m_SteamID : 0UL;

        public string Language => m_Initialized() ? SteamApps.GetCurrentGameLanguage() : null;

        private void OnAuthorization(MicroTxnAuthorizationResponse_t response) =>
            AuthorizationReceived?.Invoke(response.m_unAppID, response.m_ulOrderID, response.m_bAuthorized != 0);

        public void Dispose() => m_Authorization.Dispose();
    }
}
#endif
