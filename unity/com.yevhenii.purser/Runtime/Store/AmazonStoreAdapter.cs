using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>Status values of Amazon Appstore SDK responses.</summary>
    public static class AmazonStatus
    {
        /// <summary>The request succeeded.</summary>
        public const string Successful = "SUCCESSFUL";
        /// <summary>The request failed (or the player cancelled the purchase).</summary>
        public const string Failed = "FAILED";
        /// <summary>The SKU is not available in Amazon's catalog.</summary>
        public const string InvalidSku = "INVALID_SKU";
        /// <summary>The player already owns this entitlement or subscription.</summary>
        public const string AlreadyPurchased = "ALREADY_PURCHASED";
        /// <summary>The call is not supported on this device.</summary>
        public const string NotSupported = "NOT_SUPPORTED";
    }

    /// <summary>Amazon <c>productType</c> values.</summary>
    public static class AmazonProductType
    {
        /// <summary>A consumable.</summary>
        public const string Consumable = "CONSUMABLE";
        /// <summary>An entitlement (non-consumable).</summary>
        public const string Entitled = "ENTITLED";
        /// <summary>A subscription.</summary>
        public const string Subscription = "SUBSCRIPTION";
    }

    /// <summary>Amazon <c>PurchaseReceipt</c>.</summary>
    public sealed class AmazonReceipt
    {
        /// <summary>Receipt id the server verifies with Amazon's Receipt Verification Service.</summary>
        public string ReceiptId;
        /// <summary>Purchased SKU.</summary>
        public string Sku;
        /// <summary><see cref="AmazonProductType"/>.</summary>
        public string ProductType;
        /// <summary>Purchase time, Unix ms.</summary>
        public long PurchaseDate;
        /// <summary>0 = not canceled.</summary>
        public long CancelDate;
    }

    /// <summary>Amazon <c>ProductData</c>.</summary>
    public sealed class AmazonProduct
    {
        /// <summary>Product SKU.</summary>
        public string Sku;
        /// <summary><see cref="AmazonProductType"/>.</summary>
        public string ProductType;
        /// <summary>Localized price as Amazon formats it, e.g. <c>$0.99</c>.</summary>
        public string Price;
        /// <summary>Localized title.</summary>
        public string Title;
        /// <summary>Localized description.</summary>
        public string Description;
    }

    /// <summary>Amazon <c>GetUserDataResponse</c>.</summary>
    public sealed class AmazonUserDataResponse
    {
        /// <summary><see cref="AmazonStatus"/>.</summary>
        public string Status;
        /// <summary>Amazon user id of the signed-in account.</summary>
        public string UserId;
        /// <summary>Amazon marketplace, e.g. <c>US</c>.</summary>
        public string Marketplace;
    }

    /// <summary>Amazon <c>GetProductDataResponse</c>.</summary>
    public sealed class AmazonProductDataResponse
    {
        /// <summary><see cref="AmazonStatus"/>.</summary>
        public string Status;
        /// <summary>Products Amazon returned.</summary>
        public List<AmazonProduct> Products = new List<AmazonProduct>();
        /// <summary>Requested SKUs Amazon does not sell.</summary>
        public List<string> UnavailableSkus = new List<string>();
    }

    /// <summary>Amazon <c>PurchaseResponse</c>.</summary>
    public sealed class AmazonPurchaseResponse
    {
        /// <summary><see cref="AmazonStatus"/>.</summary>
        public string Status;
        /// <summary>Amazon user id of the buyer.</summary>
        public string UserId;
        /// <summary>The SKU that was requested (to report failures).</summary>
        public string Sku;
        /// <summary>The receipt when <see cref="Status"/> is <see cref="AmazonStatus.Successful"/>.</summary>
        public AmazonReceipt Receipt;
    }

    /// <summary>Amazon <c>GetPurchaseUpdatesResponse</c> (one page).</summary>
    public sealed class AmazonPurchaseUpdatesResponse
    {
        /// <summary><see cref="AmazonStatus"/>.</summary>
        public string Status;
        /// <summary>Amazon user id of the signed-in account.</summary>
        public string UserId;
        /// <summary>Receipts of this page.</summary>
        public List<AmazonReceipt> Receipts = new List<AmazonReceipt>();
        /// <summary>More pages follow: the adapter asks for them.</summary>
        public bool HasMore;
    }

    /// <summary>
    /// The calls of the Amazon Appstore SDK the billing package needs. Unity IAP 5 dropped its Amazon store, so
    /// the game provides this bridge over Amazon's Unity plugin (<c>AmazonIapV2Impl</c>); a ready-made one is in
    /// the "Amazon Appstore" sample. Responses are raised as events on the Unity main thread.
    /// </summary>
    public interface IAmazonIap
    {
        /// <summary>Answer of <see cref="GetUserData"/>.</summary>
        event Action<AmazonUserDataResponse> UserDataReceived;
        /// <summary>Answer of <see cref="GetProductData"/>.</summary>
        event Action<AmazonProductDataResponse> ProductDataReceived;
        /// <summary>Answer of <see cref="Purchase"/>.</summary>
        event Action<AmazonPurchaseResponse> PurchaseReceived;
        /// <summary>Answer of <see cref="GetPurchaseUpdates"/>, one page at a time.</summary>
        event Action<AmazonPurchaseUpdatesResponse> PurchaseUpdatesReceived;

        /// <summary><c>GetUserData()</c>.</summary>
        void GetUserData();
        /// <summary><c>GetProductData(skus)</c>.</summary>
        void GetProductData(IReadOnlyList<string> skus);
        /// <summary><c>Purchase(sku)</c>: shows Amazon's purchase dialog.</summary>
        void Purchase(string sku);
        /// <summary><c>GetPurchaseUpdates(reset)</c>; <paramref name="reset"/> = all receipts, not only new ones.</summary>
        void GetPurchaseUpdates(bool reset);
        /// <summary><c>NotifyFulfillment(receiptId, FULFILLED | UNAVAILABLE)</c>.</summary>
        void NotifyFulfillment(string receiptId, bool fulfilled);
    }

    /// <summary>
    /// <see cref="IStoreAdapter"/> for the Amazon Appstore. The billing server verifies receipts with Amazon's
    /// Receipt Verification Service (<c>/verify/amazon</c>, receipt id + Amazon user id); after its answer the
    /// adapter calls <c>NotifyFulfillment</c>. Pass it in <see cref="BillingDependencies.Store"/>:
    /// <code>new BillingDependencies { Store = new AmazonStoreAdapter(new AmazonAppstoreSdkBridge()) }</code>
    /// </summary>
    public sealed class AmazonStoreAdapter : IStoreAdapter
    {
        private static readonly TimeSpan OperationTimeout = TimeSpan.FromSeconds(30);
        private const string FulfilledKey = "purser.amazon.fulfilled";
        /// <summary>Fulfilled receipt ids kept to skip them at the next launch.</summary>
        private const int MaxRemembered = 200;

        private readonly IAmazonIap m_Iap;
        private readonly IBillingStorage m_Storage;
        private readonly BillingLog m_Log;
        private readonly List<string> m_Fulfilled;

        private TaskCompletionSource<AmazonUserDataResponse> m_User;
        private TaskCompletionSource<AmazonProductDataResponse> m_Products;
        private TaskCompletionSource<List<AmazonReceipt>> m_Updates;
        private readonly List<AmazonReceipt> m_UpdatesBuffer = new List<AmazonReceipt>();
        private bool m_DeliverUpdates;
        /// <summary>The purchase updates request of <see cref="ConnectAsync"/>; a restore waits for it, because both
        /// share Amazon's one updates stream.</summary>
        private TaskCompletionSource<bool> m_StartupUpdates;
        private IReadOnlyList<BillingProductConfig> m_RequestedProducts;

        /// <inheritdoc/>
        public BillingPlatform Platform => BillingPlatform.Amazon;
        /// <inheritdoc/>
        public string Store => BillingStores.Amazon;

        /// <summary>Amazon user id of the signed-in Amazon account (known after <see cref="ConnectAsync"/>).</summary>
        public string UserId { get; private set; }

        /// <summary>Amazon marketplace of the user, e.g. <c>US</c>.</summary>
        public string Marketplace { get; private set; }

        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <summary>Amazon has no deferred purchases.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
        public event Action<StoreOrder, bool, string> OrderFinished;

        /// <param name="iap">Bridge to the Amazon Appstore SDK.</param>
        /// <param name="storage">Remembers fulfilled receipts across launches (default: PlayerPrefs).</param>
        public AmazonStoreAdapter(IAmazonIap iap, IBillingStorage storage = null, BillingLog log = null)
        {
            m_Iap = iap ?? throw new ArgumentNullException(nameof(iap));
            m_Storage = storage ?? new PlayerPrefsBillingStorage();
            m_Log = log ?? new BillingLog(BillingLogLevel.Warning);
            m_Fulfilled = LoadFulfilled();
            m_Iap.UserDataReceived += HandleUserData;
            m_Iap.ProductDataReceived += HandleProductData;
            m_Iap.PurchaseReceived += HandlePurchase;
            m_Iap.PurchaseUpdatesReceived += HandlePurchaseUpdates;
        }

        /// <summary>Gets the Amazon user, then raises unfinished purchases of earlier sessions through
        /// <see cref="OrderPending"/>.</summary>
        public async Task ConnectAsync(CancellationToken ct)
        {
            m_User = new TaskCompletionSource<AmazonUserDataResponse>();
            m_Iap.GetUserData();
            var user = await TaskTimeout.With(m_User.Task, OperationTimeout, ct, "Amazon user data");
            if (user.Status != AmazonStatus.Successful || string.IsNullOrEmpty(user.UserId))
                throw new BillingException($"Amazon Appstore user data: {user.Status}");
            UserId = user.UserId;
            Marketplace = user.Marketplace;
            // Unfinished purchases of earlier sessions: delivered through OrderPending like Unity IAP does.
            m_DeliverUpdates = true;
            m_StartupUpdates = new TaskCompletionSource<bool>();
            m_UpdatesBuffer.Clear();
            m_Iap.GetPurchaseUpdates(false);
        }

        /// <inheritdoc/>
        public async Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            m_RequestedProducts = products;
            var skus = products.Select(p => p.StoreId(Store)).Distinct().ToList();
            m_Products = new TaskCompletionSource<AmazonProductDataResponse>();
            m_Iap.GetProductData(skus);
            var response = await TaskTimeout.With(m_Products.Task, OperationTimeout, ct, "Amazon product data");
            if (response.Status != AmazonStatus.Successful)
                throw new BillingException($"Amazon product data: {response.Status}");
            if (response.UnavailableSkus?.Count > 0)
                m_Log.Warning("Amazon SKUs not available: " + string.Join(", ", response.UnavailableSkus));
            var result = new List<StoreProduct>();
            foreach (var p in response.Products ?? new List<AmazonProduct>())
            {
                var config = FindConfig(p.Sku);
                result.Add(new StoreProduct(config?.id ?? p.Sku, p.Sku, config?.type ?? ToType(p.ProductType), p.Price,
                    ParsePrice(p.Price), null, p.Title, p.Description, true, p));
            }
            return result;
        }

        /// <summary>Amazon has no account binding field; the ledger keeps consumables with their buyer.</summary>
        public void SetAccountBinding(string userId) { }

        /// <inheritdoc/>
        public void Purchase(string storeProductId) => m_Iap.Purchase(storeProductId);

        /// <summary><c>NotifyFulfillment</c>: FULFILLED when the server granted the purchase, UNAVAILABLE when it
        /// rejected the receipt.</summary>
        public void Finish(StoreOrder order, bool granted)
        {
            if (string.IsNullOrEmpty(order?.TransactionId))
                return;
            m_Iap.NotifyFulfillment(order.TransactionId, granted);
            Remember(order.TransactionId);
            OrderFinished?.Invoke(order, true, null);
        }

        /// <summary>All receipts Amazon reports for the user that are not canceled.</summary>
        public async Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct)
        {
            // Amazon has one purchase updates stream: let the startup request finish first.
            var startup = m_StartupUpdates;
            if (startup != null && !startup.Task.IsCompleted)
            {
                try
                {
                    await TaskTimeout.With(startup.Task, OperationTimeout, ct, "Amazon purchase updates at startup");
                }
                catch (BillingException e)
                {
                    m_Log.Warning(e.Message);
                }
            }
            m_Updates = new TaskCompletionSource<List<AmazonReceipt>>();
            m_DeliverUpdates = false;
            m_UpdatesBuffer.Clear();
            m_Iap.GetPurchaseUpdates(true);
            var receipts = await TaskTimeout.With(m_Updates.Task, OperationTimeout, ct, "Amazon purchase updates");
            return receipts.Where(r => r.CancelDate == 0).Select(Map).Where(o => o != null).ToList();
        }

        /// <summary>Restoring on Amazon is <see cref="FetchPurchasesAsync"/> (all owned receipts).</summary>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        /// <summary>Unsubscribes from the bridge.</summary>
        public void Dispose()
        {
            m_Iap.UserDataReceived -= HandleUserData;
            m_Iap.ProductDataReceived -= HandleProductData;
            m_Iap.PurchaseReceived -= HandlePurchase;
            m_Iap.PurchaseUpdatesReceived -= HandlePurchaseUpdates;
        }

        // ---- Amazon responses ----

        private void HandleUserData(AmazonUserDataResponse response)
        {
            if (response?.Status == AmazonStatus.Successful && !string.IsNullOrEmpty(response.UserId))
                UserId = response.UserId;
            m_User?.TrySetResult(response ?? new AmazonUserDataResponse { Status = AmazonStatus.Failed });
        }

        private void HandleProductData(AmazonProductDataResponse response) =>
            m_Products?.TrySetResult(response ?? new AmazonProductDataResponse { Status = AmazonStatus.Failed });

        private void HandlePurchase(AmazonPurchaseResponse response)
        {
            if (!string.IsNullOrEmpty(response?.UserId))
                UserId = response.UserId;
            var receipt = response?.Receipt;
            if (response?.Status == AmazonStatus.Successful && receipt != null)
            {
                var order = Map(receipt);
                if (order != null)
                {
                    OrderPending?.Invoke(order);
                    return;
                }
            }
            var sku = receipt?.Sku ?? response?.Sku;
            var status = response?.Status ?? AmazonStatus.Failed;
            var reason = status == AmazonStatus.AlreadyPurchased ? "already owned: restore purchases" : status;
            PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, "Amazon: " + reason, false));
        }

        private void HandlePurchaseUpdates(AmazonPurchaseUpdatesResponse response)
        {
            if (!string.IsNullOrEmpty(response?.UserId))
                UserId = response.UserId;
            if (response?.Status != AmazonStatus.Successful)
            {
                m_Log.Warning("Amazon purchase updates: " + (response?.Status ?? "no response"));
                m_UpdatesBuffer.Clear();
                if (m_DeliverUpdates)
                {
                    m_DeliverUpdates = false;
                    m_StartupUpdates?.TrySetResult(false);
                }
                else
                {
                    m_Updates?.TrySetException(new BillingException("Amazon purchase updates: " + response?.Status));
                }
                return;
            }
            m_UpdatesBuffer.AddRange(response.Receipts ?? new List<AmazonReceipt>());
            if (response.HasMore)
            {
                m_Iap.GetPurchaseUpdates(false);
                return;
            }
            var receipts = m_UpdatesBuffer.ToList();
            m_UpdatesBuffer.Clear();
            if (!m_DeliverUpdates)
            {
                m_Updates?.TrySetResult(receipts);
                return;
            }
            m_DeliverUpdates = false;
            foreach (var r in receipts.Where(r => r.CancelDate == 0 && !m_Fulfilled.Contains(r.ReceiptId)))
            {
                var order = Map(r);
                if (order != null)
                    OrderPending?.Invoke(order);
            }
            m_StartupUpdates?.TrySetResult(true);
        }

        // ---- mapping ----

        private StoreOrder Map(AmazonReceipt r)
        {
            if (r == null || string.IsNullOrEmpty(r.ReceiptId) || string.IsNullOrEmpty(r.Sku))
                return null;
            return new StoreOrder(Platform, r.Sku, r.ReceiptId, null, r, UserId);
        }

        private BillingProductConfig FindConfig(string sku)
        {
            if (m_RequestedProducts == null)
                return null;
            foreach (var p in m_RequestedProducts)
                if (p.StoreId(Store) == sku || p.Matches(sku))
                    return p;
            return null;
        }

        private static BillingProductType ToType(string productType)
        {
            switch (productType)
            {
                case AmazonProductType.Entitled: return BillingProductType.NonConsumable;
                case AmazonProductType.Subscription: return BillingProductType.Subscription;
                default: return BillingProductType.Consumable;
            }
        }

        /// <summary>Best effort: Amazon gives only a formatted price ("$0.99", "0,99 €"); 0 when unknown.</summary>
        internal static decimal ParsePrice(string formatted)
        {
            if (string.IsNullOrEmpty(formatted))
                return 0m;
            var digits = new string(formatted.Where(c => char.IsDigit(c) || c == '.' || c == ',').ToArray()).Trim('.', ',');
            if (digits.Length == 0)
                return 0m;
            var lastSep = Math.Max(digits.LastIndexOf('.'), digits.LastIndexOf(','));
            // A separator followed by exactly 1-2 digits is the decimal point; the others group thousands.
            string normalized;
            if (lastSep >= 0 && digits.Length - lastSep - 1 <= 2)
                normalized = digits.Substring(0, lastSep).Replace(".", "").Replace(",", "") + "." + digits.Substring(lastSep + 1);
            else
                normalized = digits.Replace(".", "").Replace(",", "");
            return decimal.TryParse(normalized, NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture, out var v) ? v : 0m;
        }

        private List<string> LoadFulfilled()
        {
            try
            {
                var json = m_Storage.Load(FulfilledKey);
                return string.IsNullOrEmpty(json) ? new List<string>() : JsonConvert.DeserializeObject<List<string>>(json) ?? new List<string>();
            }
            catch (Exception e)
            {
                m_Log.Warning("Could not read the fulfilled Amazon receipts (they may be verified again): " + e.Message);
                return new List<string>();
            }
        }

        private void Remember(string receiptId)
        {
            if (m_Fulfilled.Contains(receiptId))
                return;
            m_Fulfilled.Add(receiptId);
            if (m_Fulfilled.Count > MaxRemembered)
                m_Fulfilled.RemoveRange(0, m_Fulfilled.Count - MaxRemembered);
            try
            {
                m_Storage.Save(FulfilledKey, JsonConvert.SerializeObject(m_Fulfilled));
            }
            catch (Exception e)
            {
                m_Log.Warning("Could not remember the fulfilled Amazon receipt (it may be verified again at the next launch): " + e.Message);
            }
        }
    }
}
