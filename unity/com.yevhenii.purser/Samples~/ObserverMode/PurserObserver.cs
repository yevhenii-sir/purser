// Observer mode: the game keeps its own Unity IAP code and reports each purchase to Purser.
// The asmdef next to this file defines PURSER_SAMPLE_IAP4 for Unity IAP 4.x and PURSER_SAMPLE_IAP5 for 5.x.
// See README.md for where the calls go.
#if PURSER_SAMPLE_IAP4 || PURSER_SAMPLE_IAP5
using System.Collections.Generic;
using System.Linq;
using UnityEngine.Purchasing;

namespace Purser.Samples
{
    public static class PurserObserver
    {
#if PURSER_SAMPLE_IAP4
        /// <summary>
        /// Unity IAP 4, in <c>IStoreListener.ProcessPurchase</c> before it returns <c>Complete</c>: records the purchase
        /// (the game grants it as before). <c>ReportMode.RecordOnly</c> never blocks the purchase.
        /// </summary>
        public static void Report(IBillingService billing, Product product)
        {
            var purchase = From(product);
            if (purchase != null)
                _ = billing.ReportPurchaseAsync(purchase);
        }

        /// <summary>Unity IAP 4: what the player owns now (<c>IStoreController.products</c> with a receipt), for
        /// <c>ReportRestoredAsync</c> on the first launch with Purser.</summary>
        public static IEnumerable<ExternalPurchase> Owned(IStoreController controller) =>
            controller.products.all.Where(p => p.hasReceipt).Select(From).Where(p => p != null);

        private static ExternalPurchase From(Product product) =>
            ExternalPurchase.FromUnityReceipt(product.definition.storeSpecificId, product.receipt,
                product.metadata?.localizedPrice ?? 0, product.metadata?.isoCurrencyCode);
#endif

#if PURSER_SAMPLE_IAP5
        /// <summary>
        /// Unity IAP 5, in <c>OnPurchasePending(PendingOrder order)</c> before <c>ConfirmPurchase(order)</c>: records the
        /// purchase (the game grants it as before).
        /// </summary>
        public static void Report(IBillingService billing, Order order)
        {
            var purchase = From(order);
            if (purchase != null)
                _ = billing.ReportPurchaseAsync(purchase);
        }

        /// <summary>Unity IAP 5: the confirmed orders of <c>OnPurchasesFetched</c>, for <c>ReportRestoredAsync</c>.</summary>
        public static IEnumerable<ExternalPurchase> Owned(Orders orders) =>
            orders.ConfirmedOrders.Select(From).Where(p => p != null);

        private static ExternalPurchase From(Order order)
        {
            var product = order?.CartOrdered?.Items()?.FirstOrDefault()?.Product;
            var info = order?.Info;
            if (product == null || info == null)
                return null;
            var purchase = ExternalPurchase.FromUnityReceipt(product.definition.storeSpecificId, info.Receipt,
                product.metadata?.localizedPrice ?? 0, product.metadata?.isoCurrencyCode);
            // Unity IAP 5 gives the Google token directly; the receipt may lack it.
            if (purchase != null && purchase.Store == BillingStores.Google && !string.IsNullOrEmpty(info.Google?.PurchaseToken))
                purchase.PurchaseToken = info.Google.PurchaseToken;
            return purchase;
        }
#endif
    }
}
#endif
