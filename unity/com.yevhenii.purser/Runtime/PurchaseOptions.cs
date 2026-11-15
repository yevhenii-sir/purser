namespace Purser
{
    /// <summary>
    /// How a subscription change takes effect (Google Play's <c>ReplacementMode</c>; the values match Google's and
    /// Unity IAP's <c>GooglePlayReplacementMode</c>).
    /// </summary>
    public enum ReplacementMode
    {
        /// <summary>Takes effect now; the unused time of the old plan is credited (Google's default).</summary>
        WithTimeProration = 1,
        /// <summary>Takes effect now and the billing cycle stays; the price difference for the rest of the period is
        /// charged. Upgrades only.</summary>
        ChargeProratedPrice = 2,
        /// <summary>Takes effect now; the new price is charged from the next renewal.</summary>
        WithoutProration = 3,
        /// <summary>Takes effect now; the full price of the new plan is charged and a new period starts.</summary>
        ChargeFullPrice = 5,
        /// <summary>Takes effect when the old plan renews; nothing is charged now
        /// (<see cref="CustomerInfo"/> shows it as <see cref="EntitlementInfo.RenewsTo"/>).</summary>
        Deferred = 6,
    }

    /// <summary>Options of <see cref="IBillingService.PurchaseAsync(string, PurchaseOptions, System.Threading.CancellationToken)"/>.</summary>
    public sealed class PurchaseOptions
    {
        /// <summary>The paywall the purchase is made from: the server records it for the placement, variant and test
        /// (A/B test results).</summary>
        public Paywall Paywall;

        /// <summary>
        /// The store offer the player buys (Google offer id or base plan id), sent to the server as <c>offer_id</c> for
        /// revenue by offer. It does not choose the offer: Unity IAP 5 buys the first offer Google Play lists for the
        /// product, and the App Store applies the introductory offer by itself (promotional offers:
        /// <see cref="IBillingService.PurchaseWithPromotionalOfferAsync(string, string, Paywall, System.Threading.CancellationToken)"/>).
        /// </summary>
        public string OfferId;

        /// <summary>
        /// A subscription of the player this purchase replaces (upgrade or downgrade): its config product id or store
        /// id. Google Play changes the subscription with <see cref="ReplacementMode"/>; on the App Store a purchase
        /// in the same subscription group replaces the old one by itself. Other stores fail the purchase.
        /// </summary>
        public string ReplacesProductId;

        /// <summary>How the change of <see cref="ReplacesProductId"/> takes effect (Google Play).</summary>
        public ReplacementMode ReplacementMode = ReplacementMode.WithTimeProration;
    }
}
