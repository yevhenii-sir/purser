using System;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    // Promo codes: POST /redeem gives the code's product to the player like a purchase without a payment.
    public sealed partial class BillingService
    {
        /// <inheritdoc/>
        public async Task<PurchaseResult> RedeemCodeAsync(string code, CancellationToken ct = default)
        {
            if (!IsInitialized || m_Context.IsDisposed)
                return new PurchaseResult(PurchaseStatus.NotInitialized, null, message: "billing is not initialized");
            if (string.IsNullOrWhiteSpace(code))
                return new PurchaseResult(PurchaseStatus.Failed, null, message: "the code is empty");
            var user = SyncUser();
            if (user == null)
                return new PurchaseResult(PurchaseStatus.Failed, null, message: "user id is not set");
            if (!(m_Api is IPromoCodeApi api))
                return new PurchaseResult(PurchaseStatus.Failed, null, message: "the billing API client does not redeem codes (IPromoCodeApi)");

            var response = await api.RedeemAsync(new RedeemRequest { AppId = AppId, Store = BuildStore, UserId = user, Code = code.Trim(), Client = m_Client }, ct);
            var server = response.Value;
            var productId = m_Config.FindByStoreId(server?.ProductId)?.id ?? server?.ProductId;
            PurchaseResult result;
            if (response.IsHttpSuccess && (server?.Status == VerifyStatus.Success || server?.Status == VerifyStatus.Pending))
            {
                var status = server.Status == VerifyStatus.Success ? PurchaseStatus.Success : PurchaseStatus.PendingDelivery;
                result = new PurchaseResult(status, productId, server.ProductId, server.TransactionId, server.Message, server);
                // The same event as a purchase: a game that grants on the client grants it in its handler.
                RaiseRedeemed(result);
                m_Refresher.RefreshInBackground();
            }
            else if (response.HttpStatus == 404 || response.HttpStatus == 409)
            {
                result = new PurchaseResult(PurchaseStatus.Rejected, productId, server?.ProductId,
                    message: $"{response.Code}: {server?.Message}".Trim(' ', ':'));
            }
            else
            {
                result = new PurchaseResult(PurchaseStatus.Failed, productId, message: response.Code ?? response.Error);
            }
            if (m_Log.IsEnabled(BillingLogLevel.Info))
                m_Log.Info("Promo code " + result);
            return result;
        }

        private void RaiseRedeemed(PurchaseResult result)
        {
            try
            {
                m_PurchaseSucceeded?.Invoke(result);
            }
            catch (Exception e)
            {
                m_Log.Error($"{nameof(PurchaseSucceeded)} handler threw for {result}: {e}");
            }
        }
    }
}
