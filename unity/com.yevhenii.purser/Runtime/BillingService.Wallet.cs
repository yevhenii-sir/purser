using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Virtual currency: products that give currency credit it on the server; the game reads and spends it there.
    public sealed partial class BillingService
    {
        /// <inheritdoc/>
        public async Task<WalletResult> GetWalletAsync(CancellationToken ct = default)
        {
            if (!(Ready(out var user, out var notReady) is IWalletApi api))
                return notReady;
            var response = await api.GetWalletAsync(new WalletRequest { AppId = AppId, Store = BuildStore, UserId = user, Client = m_Client }, ct);
            if (response.IsHttpSuccess && response.Value != null)
                return new WalletResult(WalletStatus.Success, response.Value.Balances ?? new Dictionary<string, long>());
            return new WalletResult(WalletStatus.Failed, message: response.Code ?? response.Error);
        }

        /// <inheritdoc/>
        public async Task<WalletResult> SpendAsync(string currency, long amount, string reference, CancellationToken ct = default)
        {
            if (!(Ready(out var user, out var notReady) is IWalletApi api))
                return notReady;
            if (string.IsNullOrWhiteSpace(currency) || amount <= 0 || string.IsNullOrWhiteSpace(reference))
                return new WalletResult(WalletStatus.Rejected, message: "currency, a positive amount and a reference are needed");
            var request = new SpendRequest { AppId = AppId, Store = BuildStore, UserId = user, Currency = currency.Trim(), Amount = amount, Reference = reference.Trim(), Client = m_Client };
            var response = await api.SpendAsync(request, ct);
            var body = response.Value;
            WalletResult result;
            if (response.IsHttpSuccess && body != null)
                result = new WalletResult(WalletStatus.Success, balance: body.Balance, repeated: body.Repeated);
            else if (response.Code == ErrorCodes.InsufficientFunds)
                result = new WalletResult(WalletStatus.InsufficientFunds, message: $"{response.Code}: {body?.Message}");
            else if (response.HttpStatus >= 400 && response.HttpStatus < 500 && !response.IsRateLimited)
                result = new WalletResult(WalletStatus.Rejected, message: $"{response.Code}: {body?.Message}".Trim(' ', ':'));
            else
                result = new WalletResult(WalletStatus.Failed, message: response.Code ?? response.Error);
            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info($"Wallet spend of {amount} {currency} ({reference}): {result}");
            return result;
        }

        /// <summary>The wallet API when billing is ready for a wallet request; else null and the result to answer.</summary>
        private IWalletApi Ready(out string user, out WalletResult notReady)
        {
            user = null;
            notReady = null;
            if (!IsInitialized || m_Context.IsDisposed)
            {
                notReady = new WalletResult(WalletStatus.NotInitialized, message: "billing is not initialized");
                return null;
            }
            user = SyncUser();
            if (user == null)
            {
                notReady = new WalletResult(WalletStatus.NotInitialized, message: "user id is not set");
                return null;
            }
            if (!(m_Api is IWalletApi api))
            {
                notReady = new WalletResult(WalletStatus.Failed, message: "the billing API client has no wallet (IWalletApi)");
                return null;
            }
            return api;
        }
    }
}
