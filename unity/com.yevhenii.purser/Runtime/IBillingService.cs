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
        /// <summary>The store is connected and the products are loaded. False before <see cref="InitializeAsync"/>
        /// completed and while the store is disconnected (<see cref="StoreDisconnected"/>).</summary>
        bool IsInitialized { get; }

        BillingPlatform Platform { get; }

        /// <summary>The config profile this service runs with (<see cref="BillingConfig.ProfileName"/>); empty when
        /// none.</summary>
        string Profile { get; }

        /// <summary>Store id of the billing server the purchases go to (<see cref="BillingStores"/>).</summary>
        string Store { get; }

        /// <summary>Products loaded from the store (localized prices, subscription offers).</summary>
        IReadOnlyList<StoreProduct> Products { get; }

        /// <summary>Last known customer info: access levels, subscriptions, non-consumables (from the server or the
        /// local cache).</summary>
        CustomerInfo CustomerInfo { get; }

        event Action Initialized;

        /// <summary>Server accepted a purchase (also for purchases finished in the background or after restart).
        /// A purchase the game grants itself (<see cref="PurchaseResult.GrantOnClient"/>) that was verified while
        /// nobody listened is kept, not finished in the store, and given to the first handler added here.</summary>
        event Action<PurchaseResult> PurchaseSucceeded;

        /// <summary>Payment or verification is still pending.</summary>
        event Action<PurchaseResult> PurchasePending;

        event Action<PurchaseResult> PurchaseFailed;

        /// <summary>What the customer info gives changed (a purchase, a renewal, an expiry, another player); not raised
        /// when a refresh brought the same items.</summary>
        event Action<CustomerInfo> CustomerInfoChanged;

        /// <summary>The store connection dropped after initialization (the reason, for logs). <see cref="IsInitialized"/>
        /// is false until <see cref="StoreReconnected"/>; the service reconnects by itself with backoff.</summary>
        event Action<string> StoreDisconnected;

        /// <summary>The store is connected again after <see cref="StoreDisconnected"/>; the customer info is refreshed.</summary>
        event Action StoreReconnected;

        /// <summary>The server accepted a purchase the game reported (<see cref="ReportPurchaseAsync"/>) or one granted
        /// provisionally (<see cref="PurchaseResult.Provisional"/>); also for purchases verified after a restart.</summary>
        event Action<PurchaseResult> PurchaseVerified;

        /// <summary>The server definitively refused a purchase the game reported (<see cref="ReportPurchaseAsync"/>) or one
        /// granted provisionally: invalid, refunded, belongs to another player. The game may take back what it gave.</summary>
        event Action<PurchaseResult> PurchaseRejected;

        /// <summary>
        /// Observer mode: the game buys with its own store code (Unity IAP 4 or 5, a store SDK) and reports each
        /// purchase here, in <c>ProcessPurchase</c> / <c>OnPurchasePending</c>, before it finishes the purchase in the
        /// store. Purser keeps the purchase on the device until the server answered (it survives a restart), verifies
        /// it with <c>client_finished</c> (the server leaves the store alone) and raises <see cref="PurchaseVerified"/>
        /// or <see cref="PurchaseRejected"/>. Use it with <see cref="ExternalStoreAdapter"/> and a build whose delivery
        /// is <c>none</c>. See docs/observer-mode.md.
        /// </summary>
        /// <returns><see cref="ReportMode.RecordOnly"/>: the server's answer when it came (the game need not wait).
        /// <see cref="ReportMode.VerifyFirst"/>: the answer, or a provisional success after
        /// <see cref="BillingConfig.failOpenAfterSeconds"/>. <see cref="PurchaseStatus.PendingVerification"/> while the
        /// server cannot be reached (the purchase stays kept), <see cref="PurchaseStatus.Failed"/> for a purchase the
        /// server cannot verify (no token / transaction id).</returns>
        Task<PurchaseResult> ReportPurchaseAsync(ExternalPurchase purchase, ReportMode mode = ReportMode.RecordOnly, CancellationToken ct = default);

        /// <summary>
        /// Observer mode: tells the server about purchases the player already owns (the store's restore / fetched
        /// purchases, e.g. on the first launch with Purser), so they show up in the customer info and the server
        /// follows their renewals and refunds. Returns the customer info after it.
        /// </summary>
        Task<CustomerInfo> ReportRestoredAsync(IEnumerable<ExternalPurchase> purchases, CancellationToken ct = default);

        /// <summary>
        /// Moves the items the player bought through the old client-side Xsolla integration (device-id login, Xsolla
        /// inventory) to the current player: <paramref name="xsollaLoginToken"/> is that login's token
        /// (<c>XsollaToken.AccessToken</c> of the Xsolla SDK). The server reads the inventory from Xsolla and records the
        /// non-consumables of the Xsolla build as owned. Call it once, in a version that still has the old login; the
        /// customer info is refreshed after it. Needs an <see cref="XsollaStoreAdapter"/> (directly or in a
        /// <see cref="CompositeStoreAdapter"/>).
        /// </summary>
        /// <exception cref="BillingException">No Xsolla store, no player id, or the server refused the token.</exception>
        Task<XsollaClaimResponse> ClaimXsollaPurchasesAsync(string xsollaLoginToken, CancellationToken ct = default);

        /// <summary>Connects to the store and loads products. Safe to call once; later calls return the same task.
        /// When the store or the network fails, the task fails and the service keeps trying by itself (with backoff,
        /// and when the app returns to the foreground) until <see cref="Initialized"/>; calling it again tries now.</summary>
        Task InitializeAsync(CancellationToken ct = default);

        /// <summary>Sets the current player (replaces the user id provider with a static one). The server accepts ids of
        /// 1-128 ASCII letters, digits and <c>- _ . :</c> (<see cref="AccountIds.IsValidUserId"/>): an e-mail address, for
        /// example, is not one. Throws <see cref="System.ArgumentException"/> for another id.</summary>
        void SetUserId(string userId);

        /// <summary>
        /// Records whether the player agreed to share consumption data with Apple when they ask the
        /// App Store for a refund. The answer is stored on the device and sent with the next purchase
        /// verification or restore; the server answers Apple's refund requests only for players who
        /// consented (and only when the build's "Refund requests" setting is on). Ask with your own
        /// prompt, not the App Tracking Transparency one.
        /// </summary>
        void SetRefundDataConsent(bool consented);

        /// <summary>
        /// Sets custom attributes of the current player (e.g. <c>email</c>, <c>level</c>), shown on the admin page's player
        /// profile. Keys are <c>a-z</c>, <c>0-9</c> and <c>_</c> (up to 40 characters, at most 30 per player); values are
        /// strings, numbers or booleans; <c>null</c> or <c>""</c> removes a key. Only send what the player agreed to share.
        /// Returns false when the user id is not set or the server refused the update (logged).
        /// </summary>
        Task<bool> SetCustomAttributesAsync(IDictionary<string, object> attributes, CancellationToken ct = default);

        StoreProduct GetProduct(string productId);

        /// <summary>Buys a product. Cancelling <paramref name="ct"/> stops waiting for the result; the store purchase goes
        /// on and its result still arrives through the events.</summary>
        Task<PurchaseResult> PurchaseAsync(string productId, CancellationToken ct = default);

        /// <summary>Same as <see cref="PurchaseAsync(string, CancellationToken)"/>, and the server records the
        /// purchase for the paywall's placement, variant and test (A/B test results).</summary>
        Task<PurchaseResult> PurchaseAsync(string productId, Paywall paywall, CancellationToken ct = default);

        /// <summary>Same as <see cref="PurchaseAsync(string, CancellationToken)"/> with a paywall, the store offer the
        /// player buys (<c>offer_id</c> for revenue by offer) and a subscription it replaces
        /// (<see cref="PurchaseOptions"/>).</summary>
        Task<PurchaseResult> PurchaseAsync(string productId, PurchaseOptions options, CancellationToken ct = default);

        /// <summary>
        /// Redeems a promo code of the app (admin page → the app → Promo codes): the server gives its product to the
        /// player once, without a payment. <see cref="PurchaseSucceeded"/> fires as for a purchase (grant it there when
        /// <see cref="PurchaseResult.GrantOnClient"/>). A code that does not exist, expired, was used up or was already
        /// redeemed by this player is <see cref="PurchaseStatus.Rejected"/>, its <see cref="PurchaseResult.Message"/>
        /// starting with the code (<see cref="ErrorCodes.CodeInvalid"/>, ...); redeeming a code again answers its grant.
        /// </summary>
        Task<PurchaseResult> RedeemCodeAsync(string code, CancellationToken ct = default);

        /// <summary>"Restore purchases": re-verifies what the store reports and returns fresh customer info (purchases
        /// that are being verified at that moment included).</summary>
        Task<CustomerInfo> RestoreAsync(CancellationToken ct = default);

        /// <summary>Asks the server for the customer info; the cached copy (<see cref="CustomerInfo.IsFromCache"/>) when it
        /// cannot be reached or its answer is not trusted (signed responses). One request runs at a time: calls made
        /// while it runs share the one request that follows it. Cancelling <paramref name="ct"/> stops waiting for the
        /// answer; the request goes on and its answer is still applied.</summary>
        Task<CustomerInfo> RefreshCustomerInfoAsync(CancellationToken ct = default);

        /// <summary>The player has this access level (e.g. <c>premium</c>) now: check levels rather than product ids.
        /// Expiring access counts only while the customer info is fresh (<see cref="BillingConfig.entitlementsMaxAgeDays"/>).</summary>
        bool HasAccess(string accessLevel);

        /// <summary>Owned non-consumable or active subscription (any platform id of the configured product).</summary>
        bool IsActive(string productId);

        /// <summary>Subscription expiration in UTC; null when not subscribed, never expires, or the customer info is too
        /// old to trust (<see cref="BillingConfig.entitlementsMaxAgeDays"/>, like <see cref="IsActive"/>).</summary>
        DateTime? GetExpiration(string productId);

        /// <summary>
        /// Opens the store's page where the player cancels or changes subscriptions (stores require apps to offer
        /// one): Google Play's subscriptions page, for one subscription when <paramref name="productId"/> names it,
        /// the App Store's subscriptions page, or Stripe's Customer Portal (a session from the server on builds with
        /// player tokens, else <see cref="StripeCheckoutOptions.CustomerPortalLoginUrl"/>). Returns false when there
        /// is no such page (Amazon Appstore, Steam and Xsolla: players manage those in the store's app or account).
        /// </summary>
        Task<bool> OpenManageSubscriptionsAsync(string productId = null, CancellationToken ct = default);

        /// <summary>
        /// App Store: shows StoreKit's sheet where the player redeems an offer code (iOS 14 and later). A redeemed code
        /// arrives as a purchase and is verified like any other (<see cref="PurchaseSucceeded"/>). Returns false on other
        /// stores, before initialization and in the Editor.
        /// </summary>
        bool PresentOfferCodeRedemption();

        /// <summary>
        /// App Store only: buys a subscription with a promotional offer (a discount for current or lapsed
        /// subscribers set up in App Store Connect). The server signs the offer for the current player
        /// (<c>POST /offers/apple/sign</c>), then the purchase goes through Unity IAP and the server as usual (with the
        /// offer id as <c>offer_id</c>). Throws <see cref="BillingException"/> on Google Play, where offers are picked on
        /// the client by their offer token. In the Editor the product is bought without the offer.
        /// </summary>
        Task<PurchaseResult> PurchaseWithPromotionalOfferAsync(string productId, string offerId, CancellationToken ct = default);

        /// <summary>Same as <see cref="PurchaseWithPromotionalOfferAsync(string, string, CancellationToken)"/>, and the
        /// server records the purchase for the paywall (A/B test results).</summary>
        Task<PurchaseResult> PurchaseWithPromotionalOfferAsync(string productId, string offerId, Paywall paywall,
            CancellationToken ct = default);

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

        /// <summary>Reports that the paywall was shown (a view in A/B test results). Call once per display.
        /// Never throws for network problems; returns false when the view was not recorded.</summary>
        Task<bool> LogPaywallShownAsync(Paywall paywall, CancellationToken ct = default);
    }
}
