// ISamsungIap over Samsung's IAP Unity plugin (developer.samsung.com/iap, SamsungIAP.Instance).
// 1. Import the Samsung IAP Unity plugin and add its SamsungIAP script to a GameObject of the first scene.
// 2. Add PURSER_SAMSUNG_IAP to Player Settings > Scripting Define Symbols (Android).
// 3. Pass the adapter to the billing service on Galaxy Store builds:
//      var deps = new BillingDependencies { UserIdProvider = ... };
//      if (BillingPlatforms.For(config) == BillingPlatform.Samsung)
//          deps.Store = new SamsungStoreAdapter(new SamsungIapBridge(test: false));
//      var billing = Billing.Create(config, deps);
// Written against the plugin's 6.x API (callbacks with a value object and an ErrorVo); Samsung's page documents the
// calls but not the value classes, so check the names below against your plugin version (CI does not build it).
#if PURSER_SAMSUNG_IAP
using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Samsung;

namespace Purser.Samples
{
    public sealed class SamsungIapBridge : ISamsungIap
    {
        private readonly bool m_Test;

        /// <param name="test">OPERATION_MODE_TEST: licensed testers buy without paying (the server records them as
        /// sandbox purchases, which the build must allow).</param>
        public SamsungIapBridge(bool test = false)
        {
            m_Test = test;
        }

        private static SamsungIAP Iap => SamsungIAP.Instance;

        public Task InitializeAsync(CancellationToken ct)
        {
            if (Iap == null)
                return Task.FromException(new BillingException("Samsung IAP: add the SamsungIAP script to a GameObject"));
            Iap.SetOperationMode(m_Test ? OperationMode.OPERATION_MODE_TEST : OperationMode.OPERATION_MODE_PRODUCTION);
            return Task.CompletedTask;
        }

        public Task<IReadOnlyList<SamsungProduct>> GetProductsAsync(IReadOnlyList<string> itemIds, CancellationToken ct)
        {
            var done = new TaskCompletionSource<IReadOnlyList<SamsungProduct>>();
            Iap.GetProductsDetails(string.Join(", ", itemIds), (list, error) =>
            {
                if (Failed(error))
                {
                    done.TrySetException(new BillingException("Samsung product details: " + Describe(error)));
                    return;
                }
                done.TrySetResult((list?.results ?? new List<ProductVo>()).Select(p => new SamsungProduct
                {
                    ItemId = p.mItemId, PriceString = p.mItemPriceString, Price = ParsePrice(p.mItemPrice), Currency = p.mCurrencyCode,
                    Name = p.mItemName, Description = p.mItemDesc,
                    SubscriptionPeriod = Period(p.mSubscriptionDurationUnit, p.mSubscriptionDurationMultiplier),
                }).ToList());
            });
            return done.Task;
        }

        public Task<SamsungPurchaseResult> PurchaseAsync(string itemId, CancellationToken ct)
        {
            var done = new TaskCompletionSource<SamsungPurchaseResult>();
            // No obfuscated account / profile ids: the billing server binds nothing for Samsung.
            Iap.StartPayment(itemId, "", "", (purchase, error) => done.TrySetResult(new SamsungPurchaseResult
            {
                ErrorCode = error?.errorCode ?? SamsungStatus.Success,
                Message = error?.errorString,
                Purchase = purchase == null ? null : new SamsungPurchase { PurchaseId = purchase.mPurchaseId, OrderId = purchase.mOrderId, ItemId = purchase.mItemId },
            }));
            return done.Task;
        }

        public Task<IReadOnlyList<SamsungPurchase>> GetOwnedPurchasesAsync(CancellationToken ct)
        {
            var done = new TaskCompletionSource<IReadOnlyList<SamsungPurchase>>();
            Iap.GetOwnedList(ItemType.all, (list, error) =>
            {
                if (Failed(error))
                {
                    done.TrySetException(new BillingException("Samsung owned purchases: " + Describe(error)));
                    return;
                }
                done.TrySetResult((list?.results ?? new List<OwnedProductVo>())
                    .Select(p => new SamsungPurchase { PurchaseId = p.mPurchaseId, ItemId = p.mItemId })
                    .ToList());
            });
            return done.Task;
        }

        public Task ConsumeAsync(string purchaseId, CancellationToken ct)
        {
            var done = new TaskCompletionSource<bool>();
            Iap.ConsumePurchasedItems(purchaseId, (list, error) =>
            {
                if (Failed(error))
                    done.TrySetException(new BillingException("Samsung consume: " + Describe(error)));
                else
                    done.TrySetResult(true);
            });
            return done.Task;
        }

        private static bool Failed(ErrorVo error) => error != null && error.errorCode != SamsungStatus.Success;

        private static string Describe(ErrorVo error) => $"{error.errorCode} {error.errorString}".Trim();

        /// <summary><c>mItemPrice</c> is a number or its text depending on the plugin version.</summary>
        private static decimal ParsePrice(object price) =>
            decimal.TryParse(Convert.ToString(price, CultureInfo.InvariantCulture), NumberStyles.Number, CultureInfo.InvariantCulture, out var d) ? d : 0m;

        /// <summary><c>MONTH</c> × 3 → <c>P3M</c>; null when unknown.</summary>
        private static string Period(string unit, object multiplier)
        {
            if (string.IsNullOrEmpty(unit))
                return null;
            var n = int.TryParse(Convert.ToString(multiplier, CultureInfo.InvariantCulture), out var m) && m > 0 ? m : 1;
            switch (unit.Trim().ToUpperInvariant())
            {
                case "DAY": return $"P{n}D";
                case "WEEK": return $"P{n}W";
                case "MONTH": return $"P{n}M";
                case "YEAR": return $"P{n}Y";
                default: return null;
            }
        }
    }
}
#endif
