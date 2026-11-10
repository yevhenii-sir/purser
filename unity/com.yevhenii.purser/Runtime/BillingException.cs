using System;

namespace Purser
{
    public class BillingException : Exception
    {
        public BillingException(string message) : base(message) { }

        public BillingException(string message, Exception inner) : base(message, inner) { }
    }

    /// <summary>The server has no enabled placement with this id for the app (<c>ERR_PLACEMENT_NOT_FOUND</c>),
    /// or its paywall was deleted (<c>ERR_PAYWALL_NOT_FOUND</c>). Show a built-in fallback screen.</summary>
    public sealed class PaywallNotFoundException : BillingException
    {
        public string PlacementId { get; }

        /// <summary>Server error code.</summary>
        public string Code { get; }

        public PaywallNotFoundException(string placementId, string code, string message)
            : base($"[{code}] {message}")
        {
            PlacementId = placementId;
            Code = code;
        }
    }
}
