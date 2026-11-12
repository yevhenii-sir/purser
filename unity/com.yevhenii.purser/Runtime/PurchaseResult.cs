namespace Purser
{
    /// <summary>Outcome of a purchase (<see cref="PurchaseResult.Status"/>).</summary>
    public enum PurchaseStatus
    {
        /// <summary>Verified by the server and the reward was delivered to the game.</summary>
        Success,
        /// <summary>Verified by the server; the reward is being delivered (server retries in the background).</summary>
        PendingDelivery,
        /// <summary>Paid in the store, but the server could not be reached yet. Verification is retried.</summary>
        PendingVerification,
        /// <summary>Payment not finished (slow card, cash, Ask to Buy). Delivered later by the store.</summary>
        PendingPayment,
        /// <summary>The player closed the store's purchase dialog or declined the payment.</summary>
        Cancelled,
        /// <summary>The server refused the purchase (invalid, belongs to another player, refunded, expired).</summary>
        Rejected,
        /// <summary>The store failed (product unavailable, network, billing unavailable...).</summary>
        Failed,
        /// <summary>A purchase of the same product is already running.</summary>
        AlreadyInProgress,
        /// <summary><see cref="IBillingService.InitializeAsync"/> has not completed, or the service was disposed.</summary>
        NotInitialized,
    }

    /// <summary>Result of a purchase: what <see cref="IBillingService.PurchaseAsync(string, System.Threading.CancellationToken)"/>
    /// returns and the purchase events carry.</summary>
    public sealed class PurchaseResult
    {
        /// <summary>What happened.</summary>
        public PurchaseStatus Status { get; }

        /// <summary>Product id from <see cref="BillingConfig"/> (falls back to the store id).</summary>
        public string ProductId { get; }

        /// <summary>Product id in the store the purchase went through.</summary>
        public string StoreProductId { get; }

        /// <summary>The store's transaction / order id (the server's when it answered).</summary>
        public string TransactionId { get; }

        /// <summary>Why it failed or is pending (for logs, not for players).</summary>
        public string Message { get; }

        /// <summary>Raw server answer when there was one.</summary>
        public VerifyResponse Server { get; }

        /// <summary>The player paid and the server accepted the purchase (reward delivered or on its way).</summary>
        public bool IsSuccess => Status == PurchaseStatus.Success || Status == PurchaseStatus.PendingDelivery;

        /// <summary>
        /// The game has no backend and must grant the reward itself (server <c>delivery = none</c>).
        /// Grant it inside <see cref="IBillingService.PurchaseSucceeded"/>, once per <see cref="TransactionId"/>:
        /// the store purchase is finished only after the handlers ran, so a crash in between makes the
        /// store deliver the same purchase again.
        /// </summary>
        public bool GrantOnClient => Server?.Delivery == VerifyDelivery.Client;

        /// <summary>Creates a result (custom adapters, tests).</summary>
        public PurchaseResult(PurchaseStatus status, string productId, string storeProductId = null,
            string transactionId = null, string message = null, VerifyResponse server = null)
        {
            Status = status;
            ProductId = productId;
            StoreProductId = storeProductId;
            TransactionId = transactionId;
            Message = message;
            Server = server;
        }

        /// <inheritdoc/>
        public override string ToString() => $"{Status} {ProductId} {TransactionId} {Message}".Trim();
    }
}
