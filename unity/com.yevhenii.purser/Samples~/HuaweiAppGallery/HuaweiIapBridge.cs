// IHuaweiIap over the HMS Unity Plugin (github.com/EvilMindDevs/hms-unity-plugin, HMSIAPManager).
// 1. Import the HMS Unity Plugin and enable In-App Purchases in its Kit Settings (agconnect-services.json in place).
// 2. Add PURSER_HMS_PLUGIN to Player Settings > Scripting Define Symbols (Android).
// 3. Pass the adapter to the billing service on Huawei builds:
//      var deps = new BillingDependencies { UserIdProvider = ... };
//      if (BillingPlatforms.For(config) == BillingPlatform.Huawei)
//          deps.Store = new HuaweiStoreAdapter(new HuaweiIapBridge());
//      var billing = Billing.Create(config, deps);
// Written against HMSIAPManager of plugin 2.x; check the names below against your plugin version (CI does not build it).
#if PURSER_HMS_PLUGIN
using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using HmsPlugin;
using HuaweiMobileServices.IAP;

namespace Purser.Samples
{
    public sealed class HuaweiIapBridge : IHuaweiIap
    {
        private static HMSIAPManager Iap => HMSIAPManager.Instance;

        public Task InitializeAsync(CancellationToken ct)
        {
            var done = new TaskCompletionSource<bool>();
            Iap.OnInitializeIAPSuccess = () => done.TrySetResult(true);
            Iap.OnInitializeIAPFailure = e => done.TrySetException(new BillingException("HMS IAP is not available: " + e?.WrappedExceptionMessage));
            Iap.InitializeIAP();
            return done.Task;
        }

        public Task<IReadOnlyList<HuaweiProduct>> GetProductsAsync(HuaweiPriceType type, IReadOnlyList<string> productIds, CancellationToken ct)
        {
            var done = new TaskCompletionSource<IReadOnlyList<HuaweiProduct>>();
            Iap.OnObtainProductInfoSuccess = results => done.TrySetResult(results
                .SelectMany(r => r.ProductInfoList)
                .Select(p => new HuaweiProduct
                {
                    ProductId = p.ProductId, PriceType = (HuaweiPriceType)p.PriceType.Value, Price = p.Price, MicrosPrice = p.MicrosPrice,
                    Currency = p.Currency, ProductName = p.ProductName, ProductDesc = p.ProductDesc, SubPeriod = p.SubPeriod,
                })
                .ToList());
            Iap.OnObtainProductInfoFailure = e => done.TrySetException(new BillingException("HMS product info: " + e?.WrappedExceptionMessage));
            var ids = productIds.ToList();
            var none = new List<string>();
            Iap.GetProductInfo(type == HuaweiPriceType.Consumable ? ids : none, type == HuaweiPriceType.NonConsumable ? ids : none,
                type == HuaweiPriceType.Subscription ? ids : none);
            return done.Task;
        }

        public Task<HuaweiPurchaseResult> PurchaseAsync(string productId, HuaweiPriceType type, string developerPayload, CancellationToken ct)
        {
            var done = new TaskCompletionSource<HuaweiPurchaseResult>();
            Iap.OnBuyProductSuccess = r => done.TrySetResult(new HuaweiPurchaseResult { ReturnCode = r.ReturnCode, Purchase = Map(r.InAppPurchaseData) });
            Iap.OnBuyProductFailure = code => done.TrySetResult(new HuaweiPurchaseResult { ReturnCode = code });
            // consume: false — the adapter consumes after the billing server granted the purchase.
            Iap.PurchaseProduct(productId, developerPayload ?? "", false);
            return done.Task;
        }

        public Task<IReadOnlyList<HuaweiPurchase>> GetOwnedPurchasesAsync(HuaweiPriceType type, CancellationToken ct)
        {
            var done = new TaskCompletionSource<IReadOnlyList<HuaweiPurchase>>();
            Iap.OnObtainOwnedPurchasesSuccess = r => done.TrySetResult(r.InAppPurchaseDataList.Select(Map).ToList());
            Iap.OnObtainOwnedPurchasesFailure = e => done.TrySetException(new BillingException("HMS owned purchases: " + e?.WrappedExceptionMessage));
            Iap.ObtainOwnedPurchases(type == HuaweiPriceType.Subscription ? PriceType.IN_APP_SUBSCRIPTION
                : type == HuaweiPriceType.NonConsumable ? PriceType.IN_APP_NONCONSUMABLE : PriceType.IN_APP_CONSUMABLE);
            return done.Task;
        }

        public Task ConsumeAsync(string purchaseToken, CancellationToken ct)
        {
            var done = new TaskCompletionSource<bool>();
            Iap.OnConsumePurchaseSuccess = _ => done.TrySetResult(true);
            Iap.OnConsumePurchaseFailure = e => done.TrySetException(new BillingException("HMS consume: " + e?.WrappedExceptionMessage));
            Iap.ConsumePurchaseWithToken(purchaseToken);
            return done.Task;
        }

        private static HuaweiPurchase Map(InAppPurchaseData d) => d == null ? null : new HuaweiPurchase
        {
            OrderId = d.OrderID, PurchaseToken = d.PurchaseToken, ProductId = d.ProductId, PriceType = (HuaweiPriceType)d.Kind,
            SubscriptionId = d.SubscriptionId, PurchaseState = d.PurchaseState,
        };
    }
}
#endif
