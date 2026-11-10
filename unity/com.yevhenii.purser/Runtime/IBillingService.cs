using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// In-app purchases verified by the Purser billing server.
    /// Rewards are delivered by the server to the game backend; the client only reacts to results.
    /// </summary>
    public interface IBillingService : IDisposable
    {
        bool IsInitialized { get; }

        BillingPlatform Platform { get; }

        /// <summary>Store id of the billing server the purchases go to (<see cref="BillingStores"/>).</summary>
        string Store { get; }

        /// <summary>Products loaded from the store (localized prices).</summary>
        IReadOnlyList<StoreProduct> Products { get; }

        /// <summary>Last known entitlements (from the server or the local cache).</summary>
        Entitlements Entitlements { get; }

        event Action Initialized;

        /// <summary>Server accepted a purchase (also for purchases finished in the background or after restart).</summary>
        event Action<PurchaseResult> PurchaseSucceeded;

        /// <summary>Payment or verification is still pending.</summary>
        event Action<PurchaseResult> PurchasePending;

        event Action<PurchaseResult> PurchaseFailed;

        event Action<Entitlements> EntitlementsChanged;

        /// <summary>Connects to the store and loads products. Safe to call once; later calls return the same task.</summary>
        Task InitializeAsync(CancellationToken ct = default);

        /// <summary>Sets the current player (replaces the user id provider with a static one).</summary>
        void SetUserId(string userId);

        /// <summary>
        /// Records whether the player agreed to share consumption data with Apple when they ask the
        /// App Store for a refund. The answer is stored on the device and sent with the next purchase
        /// verification or restore; the server answers Apple's refund requests only for players who
        /// consented (and only when the build's "Refund requests" setting is on). Ask with your own
        /// prompt, not the App Tracking Transparency one.
        /// </summary>
        void SetRefundDataConsent(bool consented);

        StoreProduct GetProduct(string productId);

        Task<PurchaseResult> PurchaseAsync(string productId, CancellationToken ct = default);

        /// <summary>"Restore purchases": re-verifies what the store reports and returns fresh entitlements.</summary>
        Task<Entitlements> RestoreAsync(CancellationToken ct = default);

        Task<Entitlements> RefreshEntitlementsAsync(CancellationToken ct = default);

        /// <summary>Owned non-consumable or active subscription (any platform id of the configured product).</summary>
        bool IsActive(string productId);

        /// <summary>Subscription expiration in UTC; null when not subscribed or never expires.</summary>
        DateTime? GetExpiration(string productId);

        /// <summary>
        /// App Store only: buys a subscription with a promotional offer (a discount for current or lapsed
        /// subscribers set up in App Store Connect). The server signs the offer for the current player
        /// (<c>POST /offers/apple/sign</c>), then the purchase goes through Unity IAP and the server as usual.
        /// Throws <see cref="BillingException"/> on Google Play, where offers are picked on the client by their
        /// offer token. In the Editor the product is bought without the offer.
        /// </summary>
        Task<PurchaseResult> PurchaseWithPromotionalOfferAsync(string productId, string offerId, CancellationToken ct = default);

        /// <summary>
        /// App Store only: a server-signed promotional offer for the current player, for games that buy through
        /// their own StoreKit code (e.g. with the V2 <see cref="ApplePromotionalOffer.SignedJws"/>). Each offer is
        /// good for one purchase attempt. Throws <see cref="BillingException"/> when it cannot be signed.
        /// </summary>
        Task<ApplePromotionalOffer> GetApplePromotionalOfferAsync(string productId, string offerId, CancellationToken ct = default);

        /// <summary>
        /// What to show at a placement (e.g. <c>"main_paywall"</c>): ordered products with localized prices,
        /// remote config and the A/B test variant of the current player. The last answer per placement is
        /// cached for offline starts (<see cref="Paywall.IsFromCache"/>).
        /// </summary>
        /// <exception cref="PaywallNotFoundException">No enabled placement with this id on the server.</exception>
        /// <exception cref="BillingException">The server is unreachable and nothing is cached, or it refused the request.</exception>
        Task<Paywall> GetPaywallAsync(string placementId, CancellationToken ct = default);

        /// <summary>Same as <see cref="PurchaseAsync(string, CancellationToken)"/>, and the server records the
        /// purchase for the paywall's placement, variant and test (A/B test results).</summary>
        Task<PurchaseResult> PurchaseAsync(string productId, Paywall paywall, CancellationToken ct = default);

        /// <summary>Reports that the paywall was shown (a view in A/B test results). Call once per display.
        /// Never throws for network problems; returns false when the view was not recorded.</summary>
        Task<bool> LogPaywallShownAsync(Paywall paywall, CancellationToken ct = default);
    }
}
