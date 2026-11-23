using System;
using System.Threading.Tasks;

namespace Purser
{
    // Store connection
    public sealed partial class BillingService
    {
        private void OnStoreDisconnected(string reason)
        {
            if (m_Context.IsDisposed || !m_EverInitialized || m_Reconnecting)
                return;
            IsInitialized = false;
            m_Log.Warning($"Store disconnected ({reason}); reconnecting.");
            try
            {
                StoreDisconnected?.Invoke(reason);
            }
            catch (Exception e)
            {
                m_Log.Error($"{nameof(StoreDisconnected)} handler threw: {e}");
            }
            _ = ReconnectAsync();
        }

        /// <summary>Delay before reconnection attempt <paramref name="attempt"/>: 5 s, doubling, at most 5 minutes.</summary>
        internal static TimeSpan ReconnectDelay(int attempt) =>
            TimeSpan.FromSeconds(Math.Min(300, 5 * Math.Pow(2, Math.Min(attempt, 6))));

        private async Task ReconnectAsync()
        {
            m_Reconnecting = true;
            try
            {
                for (var attempt = 0; !m_Context.IsDisposed; attempt++)
                {
                    await m_Delay(ReconnectDelay(attempt), m_Context.Lifetime);
                    if (m_Context.IsDisposed)
                        return;
                    try
                    {
                        await m_Store.ConnectAsync(m_Context.Lifetime);
                    }
                    catch (OperationCanceledException) when (m_Context.IsDisposed)
                    {
                        return;
                    }
                    catch (Exception e)
                    {
                        m_Log.Warning($"Store reconnection attempt {attempt + 1} failed: {e.Message}");
                        continue;
                    }
                    if (m_Context.IsDisposed)
                        return;
                    IsInitialized = true;
                    m_Log.Info("Store reconnected.");
                    try
                    {
                        StoreReconnected?.Invoke();
                    }
                    catch (Exception e)
                    {
                        m_Log.Error($"{nameof(StoreReconnected)} handler threw: {e}");
                    }
                    m_Orders.ProcessWaitingOrders();
                    if (SyncUser(refreshOnChange: false) != null)
                        m_Refresher.RefreshInBackground();
                    return;
                }
            }
            catch (OperationCanceledException)
            {
                // Disposed.
            }
            catch (ObjectDisposedException)
            {
                // Disposed.
            }
            finally
            {
                m_Reconnecting = false;
            }
        }
    }
}
