// ISteamMicroTxnBridge over Steamworks.NET (https://github.com/rlabrecque/Steamworks.NET).
// 1. Install Steamworks.NET (20.2 or later, for GetAuthTicketForWebApi) and initialize the Steam API as usual
//    (e.g. its SteamManager component).
// 2. Add PURSER_STEAMWORKS_NET to Player Settings > Scripting Define Symbols (Standalone).
// 3. Pass the adapter to the billing service on Steam builds:
//      var bridge = new SteamworksNetBridge(() => SteamManager.Initialized);
//      var deps = new BillingDependencies { UserIdProvider = ..., Store = new SteamStoreAdapter(config, bridge) };
//      var billing = Billing.Create(config, deps);
// Steamworks.NET raises callbacks from SteamAPI.RunCallbacks (SteamManager calls it in Update), so
// AuthorizationReceived and the Web API tickets arrive on the main thread.
#if PURSER_STEAMWORKS_NET
using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Steamworks;

namespace Purser.Samples
{
    public sealed class SteamworksNetBridge : ISteamMicroTxnBridge, ISteamWebApiTicketProvider, IDisposable
    {
        private readonly Func<bool> m_Initialized;
        // Kept in fields: a collected Callback stops delivering.
        private readonly Callback<MicroTxnAuthorizationResponse_t> m_Authorization;
        private readonly Callback<GetTicketForWebApiResponse_t> m_Ticket;
        private readonly Dictionary<HAuthTicket, TaskCompletionSource<string>> m_Tickets =
            new Dictionary<HAuthTicket, TaskCompletionSource<string>>();

        public event Action<uint, ulong, bool> AuthorizationReceived;

        /// <param name="initialized">Whether the game initialized the Steam API (e.g. <c>() => SteamManager.Initialized</c>).
        /// Steamworks calls before <c>SteamAPI.Init</c> crash, so the bridge asks first.</param>
        public SteamworksNetBridge(Func<bool> initialized)
        {
            m_Initialized = initialized ?? throw new ArgumentNullException(nameof(initialized));
            m_Authorization = Callback<MicroTxnAuthorizationResponse_t>.Create(OnAuthorization);
            m_Ticket = Callback<GetTicketForWebApiResponse_t>.Create(OnTicket);
        }

        public bool IsReady => m_Initialized() && SteamUser.BLoggedOn();

        public ulong SteamId => m_Initialized() ? SteamUser.GetSteamID().m_SteamID : 0UL;

        public string Language => m_Initialized() ? SteamApps.GetCurrentGameLanguage() : null;

        /// <summary>A Web API ticket for <paramref name="identity"/> as hex, or null when Steam gives none.</summary>
        public Task<string> GetWebApiTicketAsync(string identity, CancellationToken ct)
        {
            if (!m_Initialized())
                return Task.FromResult<string>(null);
            var handle = SteamUser.GetAuthTicketForWebApi(identity);
            if (handle == HAuthTicket.Invalid)
                return Task.FromResult<string>(null);
            var tcs = new TaskCompletionSource<string>();
            m_Tickets[handle] = tcs;
            ct.Register(() =>
            {
                if (m_Tickets.Remove(handle))
                    tcs.TrySetCanceled();
            });
            return tcs.Task;
        }

        private void OnAuthorization(MicroTxnAuthorizationResponse_t response) =>
            AuthorizationReceived?.Invoke(response.m_unAppID, response.m_ulOrderID, response.m_bAuthorized != 0);

        private void OnTicket(GetTicketForWebApiResponse_t response)
        {
            if (!m_Tickets.TryGetValue(response.m_hAuthTicket, out var tcs))
                return;
            m_Tickets.Remove(response.m_hAuthTicket);
            if (response.m_eResult != EResult.k_EResultOK || response.m_cubTicket <= 0)
            {
                tcs.TrySetResult(null);
                return;
            }
            var hex = new StringBuilder(response.m_cubTicket * 2);
            for (var i = 0; i < response.m_cubTicket; i++)
                hex.Append(response.m_rgubTicket[i].ToString("X2"));
            tcs.TrySetResult(hex.ToString());
        }

        public void Dispose()
        {
            m_Authorization.Dispose();
            m_Ticket.Dispose();
            foreach (var pending in m_Tickets.Values)
                pending.TrySetResult(null);
            m_Tickets.Clear();
        }
    }
}
#endif
