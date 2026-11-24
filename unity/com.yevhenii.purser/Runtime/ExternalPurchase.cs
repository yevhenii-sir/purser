using System;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Purser
{
    /// <summary>What <see cref="IBillingService.ReportPurchaseAsync"/> does with a purchase of the game's own store code.</summary>
    public enum ReportMode
    {
        /// <summary>
        /// The game granted the purchase and finishes it in the store itself, as before Purser. Purser records and
        /// verifies it in the background (also after a restart, from its own queue) and tells the game the outcome
        /// through <see cref="IBillingService.PurchaseVerified"/> / <see cref="IBillingService.PurchaseRejected"/>: the
        /// game may take back what a rejected purchase gave.
        /// </summary>
        RecordOnly,

        /// <summary>
        /// The game waits for the answer and grants on <see cref="PurchaseResult.IsSuccess"/>. When the server cannot be
        /// reached for <see cref="BillingConfig.failOpenAfterSeconds"/>, the result is a provisional success
        /// (<see cref="PurchaseResult.Provisional"/>) and the purchase is verified later from the queue.
        /// </summary>
        VerifyFirst,
    }

    /// <summary>
    /// A purchase the game's own store code made (observer mode: the game keeps its Unity IAP / store SDK code and
    /// reports each purchase with <see cref="IBillingService.ReportPurchaseAsync"/>). No store types: works with
    /// Unity IAP 4 and 5 and other SDKs. Build it with <see cref="FromUnityReceipt"/> or set the fields.
    /// </summary>
    public sealed class ExternalPurchase
    {
        /// <summary>Server store id (<see cref="BillingStores"/>): <c>google</c>, <c>apple</c> or <c>amazon</c>.</summary>
        public string Store;

        /// <summary>The product id in the store.</summary>
        public string StoreProductId;

        /// <summary>Apple transactionId, Amazon receiptId, Google orderId (optional for Google).</summary>
        public string TransactionId;

        /// <summary>Google purchase token (required for Google).</summary>
        public string PurchaseToken;

        /// <summary>Amazon userId (required for Amazon).</summary>
        public string StoreUserId;

        /// <summary>The price the store showed (localized), for revenue where the store does not report it (Google).</summary>
        public decimal Price;

        /// <summary>ISO 4217 currency of <see cref="Price"/>.</summary>
        public string Currency;

        /// <summary>Stable key: the Google token, otherwise the transaction id.</summary>
        [JsonIgnore]
        public string Key => !string.IsNullOrEmpty(PurchaseToken) ? PurchaseToken : TransactionId;

        /// <summary>
        /// From a Unity IAP receipt (<c>Product.receipt</c> in Unity IAP 4, <c>Order.Info.Receipt</c> in Unity IAP 5):
        /// <c>{"Store": "GooglePlay" | "AppleAppStore" | "MacAppStore" | "AmazonApps", "TransactionID", "Payload"}</c>.
        /// Google: the purchase token and order id from the payload; Apple, Amazon: the transaction id. Null when the
        /// receipt is not one of these (the Editor's fake store gives <see cref="Store"/> <c>fake</c>).
        /// </summary>
        /// <param name="storeProductId">The product's store id (<c>Product.definition.storeSpecificId</c>).</param>
        /// <param name="price">The localized price (<c>Product.metadata.localizedPrice</c>); 0 when unknown.</param>
        /// <param name="currency">ISO 4217 (<c>Product.metadata.isoCurrencyCode</c>).</param>
        public static ExternalPurchase FromUnityReceipt(string storeProductId, string unityReceipt, decimal price = 0, string currency = null)
        {
            JObject receipt;
            try
            {
                receipt = string.IsNullOrEmpty(unityReceipt) ? null : JObject.Parse(unityReceipt);
            }
            catch (JsonException)
            {
                return null;
            }
            if (receipt == null)
                return null;
            var p = new ExternalPurchase { StoreProductId = storeProductId, Price = price, Currency = currency };
            var unityStore = receipt["Store"]?.Value<string>() ?? "";
            switch (unityStore)
            {
                case "GooglePlay":
                    p.Store = BillingStores.Google;
                    ReceiptParser.TryGetGooglePurchaseToken(unityReceipt, out p.PurchaseToken);
                    ReceiptParser.TryGetGoogleOrderId(unityReceipt, out p.TransactionId);
                    break;
                case "AppleAppStore":
                case "MacAppStore":
                    p.Store = BillingStores.Apple;
                    ReceiptParser.TryGetTransactionId(unityReceipt, out p.TransactionId);
                    break;
                case "AmazonApps":
                    p.Store = BillingStores.Amazon;
                    ReceiptParser.TryGetTransactionId(unityReceipt, out p.TransactionId);
                    break;
                case "fake":
                    p.Store = "fake";
                    ReceiptParser.TryGetTransactionId(unityReceipt, out p.TransactionId);
                    break;
                default:
                    return null;
            }
            return string.IsNullOrEmpty(p.Key) ? null : p;
        }

        /// <summary>Why the server cannot verify it; null when it can.</summary>
        internal string Problem()
        {
            if (string.IsNullOrEmpty(StoreProductId))
                return "no store product id";
            switch (Store)
            {
                case BillingStores.Google:
                    return string.IsNullOrEmpty(PurchaseToken) ? "a Google purchase needs its purchase token" : null;
                case BillingStores.Apple:
                    return string.IsNullOrEmpty(TransactionId) ? "an App Store purchase needs its transaction id" : null;
                case BillingStores.Amazon:
                    return string.IsNullOrEmpty(TransactionId) || string.IsNullOrEmpty(StoreUserId)
                        ? "an Amazon purchase needs its receipt id and the Amazon user id"
                        : null;
                case "fake":
                    return string.IsNullOrEmpty(Key) ? "no transaction id" : null;
                default:
                    return $"store '{Store}' is not reported in observer mode (google, apple, amazon)";
            }
        }

        /// <inheritdoc/>
        public override string ToString() => $"{Store} {StoreProductId} {TransactionId}".Trim();

        internal ExternalPurchase Copy() => (ExternalPurchase)MemberwiseClone();

        [JsonIgnore]
        internal long PriceMicros => Price > 0 ? (long)decimal.Round(Price * 1_000_000m) : 0;
    }
}
