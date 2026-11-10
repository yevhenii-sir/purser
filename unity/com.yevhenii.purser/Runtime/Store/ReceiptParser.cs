using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Purser
{
    /// <summary>
    /// Fallback parsing of the Unity IAP receipt
    /// <c>{"Store":"GooglePlay","TransactionID":"…","Payload":"{\"json\":\"{…purchaseToken…}\",\"signature\":\"…\"}"}</c>
    /// for when the typed order info is not available.
    /// </summary>
    internal static class ReceiptParser
    {
        public static bool TryGetGooglePurchaseToken(string unityReceipt, out string purchaseToken)
        {
            purchaseToken = null;
            var purchase = GooglePurchaseJson(unityReceipt);
            purchaseToken = purchase?["purchaseToken"]?.Value<string>();
            return !string.IsNullOrEmpty(purchaseToken);
        }

        public static bool TryGetGoogleOrderId(string unityReceipt, out string orderId)
        {
            orderId = GooglePurchaseJson(unityReceipt)?["orderId"]?.Value<string>();
            return !string.IsNullOrEmpty(orderId);
        }

        public static bool TryGetTransactionId(string unityReceipt, out string transactionId)
        {
            transactionId = Parse(unityReceipt)?["TransactionID"]?.Value<string>();
            return !string.IsNullOrEmpty(transactionId);
        }

        private static JObject GooglePurchaseJson(string unityReceipt)
        {
            var payload = Parse(unityReceipt)?["Payload"]?.Value<string>();
            var json = Parse(payload)?["json"]?.Value<string>();
            return Parse(json);
        }

        private static JObject Parse(string s)
        {
            if (string.IsNullOrEmpty(s))
                return null;
            try
            {
                return JObject.Parse(s);
            }
            catch (JsonException)
            {
                return null;
            }
        }
    }
}
