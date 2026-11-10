using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// Optional interface of an <see cref="IStoreAdapter"/> whose store reports purchases to the billing server
    /// with webhooks (Xsolla): there is no store receipt to post to <c>/verify/{store}</c>. <see cref="BillingService"/>
    /// tells the adapter who is buying before <see cref="IStoreAdapter.Purchase"/>, and asks the adapter instead of
    /// <c>/verify</c> whether a pending order was paid and granted.
    /// </summary>
    public interface IWebhookStoreAdapter : IBillingUserStoreAdapter
    {
        /// <summary>
        /// The server's answer for an order raised through <see cref="IStoreAdapter.OrderPending"/>, in the shape of
        /// <c>/verify</c>: <see cref="VerifyStatus.Success"/> once granted, <see cref="VerifyStatus.PendingOrCanceled"/>
        /// while not paid, <see cref="VerifyStatus.Revoked"/> after a refund. A transport error is retried.
        /// </summary>
        Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, CancellationToken ct);
    }
}
