// IAmazonIap over Amazon's Appstore SDK Unity plugin (namespace com.amazon.device.iap.cpt).
// 1. Import the Amazon Appstore SDK Unity plugin (Amazon Developer Portal > Appstore SDK > Unity plugin).
// 2. Add PURSER_AMAZON_APPSTORE_SDK to Player Settings > Scripting Define Symbols (Android).
// 3. Pass the adapter to the billing service on Amazon builds:
//      var deps = new BillingDependencies { UserIdProvider = ... };
//      if (BillingPlatforms.For(config) == BillingPlatform.Amazon)
//          deps.Store = new AmazonStoreAdapter(new AmazonAppstoreSdkBridge());
//      var billing = Billing.Create(config, deps);
#if PURSER_AMAZON_APPSTORE_SDK
using System;
using System.Collections.Generic;
using System.Linq;
using com.amazon.device.iap.cpt;

namespace Purser.Samples
{
    public sealed class AmazonAppstoreSdkBridge : IAmazonIap
    {
        private readonly IAmazonIapV2 m_Iap = AmazonIapV2Impl.Instance;
        private string m_LastSku;

        public event Action<AmazonUserDataResponse> UserDataReceived;
        public event Action<AmazonProductDataResponse> ProductDataReceived;
        public event Action<AmazonPurchaseResponse> PurchaseReceived;
        public event Action<AmazonPurchaseUpdatesResponse> PurchaseUpdatesReceived;

        public AmazonAppstoreSdkBridge()
        {
            m_Iap.AddGetUserDataResponseListener(r => UserDataReceived?.Invoke(new AmazonUserDataResponse
            {
                Status = r.Status, UserId = r.AmazonUserData?.UserId, Marketplace = r.AmazonUserData?.Marketplace,
            }));
            m_Iap.AddGetProductDataResponseListener(r => ProductDataReceived?.Invoke(new AmazonProductDataResponse
            {
                Status = r.Status,
                Products = (r.ProductDataMap ?? new Dictionary<string, ProductData>()).Values.Select(p => new AmazonProduct
                {
                    Sku = p.Sku, ProductType = p.ProductType, Price = p.Price, Title = p.Title, Description = p.Description,
                }).ToList(),
                UnavailableSkus = r.UnavailableSkus ?? new List<string>(),
            }));
            m_Iap.AddPurchaseResponseListener(r => PurchaseReceived?.Invoke(new AmazonPurchaseResponse
            {
                Status = r.Status, UserId = r.AmazonUserData?.UserId, Sku = m_LastSku, Receipt = Map(r.PurchaseReceipt),
            }));
            m_Iap.AddGetPurchaseUpdatesResponseListener(r => PurchaseUpdatesReceived?.Invoke(new AmazonPurchaseUpdatesResponse
            {
                Status = r.Status,
                UserId = r.AmazonUserData?.UserId,
                Receipts = (r.Receipts ?? new List<PurchaseReceipt>()).Select(Map).ToList(),
                HasMore = r.HasMore,
            }));
        }

        public void GetUserData() => m_Iap.GetUserData();

        public void GetProductData(IReadOnlyList<string> skus) => m_Iap.GetProductData(new SkusInput { Skus = skus.ToList() });

        public void Purchase(string sku)
        {
            m_LastSku = sku;
            m_Iap.Purchase(new SkuInput { Sku = sku });
        }

        public void GetPurchaseUpdates(bool reset) => m_Iap.GetPurchaseUpdates(new ResetInput { Reset = reset });

        public void NotifyFulfillment(string receiptId, bool fulfilled) =>
            m_Iap.NotifyFulfillment(new NotifyFulfillmentInput { ReceiptId = receiptId, FulfillmentResult = fulfilled ? "FULFILLED" : "UNAVAILABLE" });

        private static AmazonReceipt Map(PurchaseReceipt r) => r == null ? null : new AmazonReceipt
        {
            ReceiptId = r.ReceiptId, Sku = r.Sku, ProductType = r.ProductType, PurchaseDate = r.PurchaseDate, CancelDate = r.CancelDate,
        };
    }
}
#endif
