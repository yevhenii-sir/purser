namespace Purser
{
    /// <summary>
    /// Optional interface of an <see cref="IStoreAdapter"/> whose purchase starts on the billing server and must
    /// name the player (Steam <c>/checkout/steam</c>, Xsolla <c>/checkout/xsolla</c>). <see cref="BillingService"/>
    /// calls <see cref="SetBillingUser"/> right before <see cref="IStoreAdapter.Purchase"/>.
    /// </summary>
    public interface IBillingUserStoreAdapter
    {
        /// <summary>Billing user id of the player about to buy (the server grants the purchase to this id).</summary>
        void SetBillingUser(string userId);
    }
}
