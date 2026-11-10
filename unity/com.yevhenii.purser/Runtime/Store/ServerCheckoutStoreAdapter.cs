using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>Who is buying, given to an <see cref="ICheckoutStoreAdapter"/> right before
    /// <see cref="IStoreAdapter.Purchase"/>: the server starts the order for this player.</summary>
    public sealed class CheckoutContext
    {
        /// <summary>Billing user id the server grants the purchase to.</summary>
        public string UserId;

        /// <summary>Device ids for attribution integrations (<see cref="AttributionKeys"/>), when the store's checkout
        /// takes them (Xsolla); others send them with the order check.</summary>
        public IReadOnlyDictionary<string, string> Attribution;
    }

    /// <summary>
    /// An <see cref="IStoreAdapter"/> whose purchase starts on the billing server (Steam <c>/checkout/steam</c>, Stripe
    /// <c>/checkout/stripe</c>, Xsolla <c>/checkout/xsolla</c>). <see cref="BillingService"/> calls
    /// <see cref="PrepareCheckout"/> right before <see cref="IStoreAdapter.Purchase"/>.
    /// </summary>
    public interface ICheckoutStoreAdapter : IStoreAdapter
    {
        void PrepareCheckout(CheckoutContext context);
    }

    /// <summary>
    /// An <see cref="ICheckoutStoreAdapter"/> whose store reports payments to the billing server (Stripe, Xsolla): there
    /// is no store receipt to post to <c>/verify/{store}</c>. <see cref="BillingService"/> asks the adapter instead whether
    /// a pending order was paid and granted.
    /// </summary>
    public interface IWebhookStoreAdapter : ICheckoutStoreAdapter
    {
        /// <summary>
        /// The server's answer for an order raised through <see cref="IStoreAdapter.OrderPending"/>, in the shape of
        /// <c>/verify</c>: <see cref="VerifyStatus.Success"/> once granted, <see cref="VerifyStatus.PendingOrCanceled"/>
        /// while not paid, <see cref="VerifyStatus.Revoked"/> after a refund. A transport error is retried.
        /// <paramref name="request"/> carries what the client knows about the purchase (attribution, paywall, app
        /// instance id) for stores whose check takes it.
        /// </summary>
        Task<ApiResult<VerifyResponse>> CheckOrderAsync(StoreOrder order, VerifyRequest request, CancellationToken ct);
    }

    /// <summary>
    /// Base of the adapters whose purchase starts on the billing server: remembers started orders across launches
    /// (re-raised on <see cref="ConnectAsync"/> until the server answers for them), lists products from the config,
    /// has nothing to restore from the store (owned products come from the server's entitlements) and no account
    /// binding (the server puts the billing user id into the order). Derive from it for another such store.
    /// </summary>
    public abstract class ServerCheckoutStoreAdapter : ICheckoutStoreAdapter
    {
        /// <summary>A started order, persisted until the server answers for it.</summary>
        protected sealed class CheckoutOrder
        {
            [JsonProperty("order_id")] public string OrderId;
            [JsonProperty("sku")] public string Sku;
            [JsonProperty("user_id")] public string UserId;
            [JsonProperty("created_ticks")] public long CreatedTicks;
        }

        private readonly IBillingStorage m_Storage;
        private readonly string m_PendingKey;
        private readonly TimeSpan m_ForgetAfter;
        private List<CheckoutOrder> m_Pending;

        /// <summary>Test hook: waits between polls.</summary>
        internal Func<TimeSpan, CancellationToken, Task> Delay = (d, ct) => Task.Delay(d, ct);

        /// <summary>Test hook: the clock.</summary>
        internal Func<DateTime> UtcNow = () => DateTime.UtcNow;

        protected BillingLog Log { get; }

        /// <summary>The player of the purchase being started (<see cref="PrepareCheckout"/>).</summary>
        protected CheckoutContext Checkout { get; private set; }

        protected string CurrentUser => string.IsNullOrEmpty(Checkout?.UserId) ? null : Checkout.UserId;

        public abstract BillingPlatform Platform { get; }
        public abstract string Store { get; }

        public event Action<StoreOrder> OrderPending;
        /// <summary>Not used: an unpaid order is reported by the order check.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        public event Action<StorePurchaseFailure> PurchaseFailed;
        public event Action<StoreOrder, bool, string> OrderFinished;

        /// <param name="pendingKey">Storage key of the started orders.</param>
        /// <param name="forgetAfter">Orders older than this are no longer re-checked at launch.</param>
        protected ServerCheckoutStoreAdapter(IBillingStorage storage, BillingLog log, string pendingKey, TimeSpan forgetAfter)
        {
            m_Storage = storage ?? new PlayerPrefsBillingStorage();
            Log = log ?? new BillingLog(BillingLogLevel.Warning);
            m_PendingKey = pendingKey;
            m_ForgetAfter = forgetAfter;
        }

        /// <summary>Re-raises orders of earlier launches the server has not answered for yet.</summary>
        public virtual Task ConnectAsync(CancellationToken ct)
        {
            var cutoff = UtcNow() - m_ForgetAfter;
            if (Pending.RemoveAll(o => new DateTime(o.CreatedTicks, DateTimeKind.Utc) < cutoff) > 0)
                SavePending();
            ResumePendingOrders();
            return Task.CompletedTask;
        }

        /// <summary>Checks the remembered orders again (e.g. when the player returns from the payment page); their
        /// results arrive as purchase events of the billing service.</summary>
        public void ResumePendingOrders()
        {
            foreach (var o in Pending.ToList())
                OrderPending?.Invoke(ToOrder(o));
        }

        /// <summary>Products from the config: prices are shown by the store's own page or overlay.</summary>
        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            IReadOnlyList<StoreProduct> list = products
                .Select(p => new StoreProduct(p.id, p.StoreId(Store), p.type, "", 0m, null, p.id, "", IsSold(p)))
                .ToList();
            return Task.FromResult(list);
        }

        /// <summary>Whether the store sells this product through the adapter.</summary>
        protected virtual bool IsSold(BillingProductConfig product) => true;

        /// <summary>No account binding field: the server puts the billing user id into the order.</summary>
        public void SetAccountBinding(string userId) { }

        public void PrepareCheckout(CheckoutContext context) => Checkout = context;

        public void Purchase(string storeProductId) => _ = PurchaseCoreAsync(storeProductId);

        private async Task PurchaseCoreAsync(string sku)
        {
            try
            {
                var user = CurrentUser;
                if (user == null)
                {
                    Fail(sku, "the billing user id is not set");
                    return;
                }
                await StartCheckoutAsync(sku, user);
            }
            catch (OperationCanceledException)
            {
                // Disposed.
            }
            catch (Exception e)
            {
                Log.Error($"{Store} checkout failed: {e}");
                Fail(sku, "checkout: " + e.Message);
            }
        }

        /// <summary>Asks the server to start the order and shows the store's page or overlay. Report the result with
        /// <see cref="Fail"/>, or <see cref="Remember"/> + <see cref="RaisePending"/>.</summary>
        protected abstract Task StartCheckoutAsync(string sku, string userId);

        /// <summary>The server answered for the order: it is no longer re-checked at launch.</summary>
        public void Finish(StoreOrder order, bool granted)
        {
            if (order?.TransactionId == null)
                return;
            if (Pending.RemoveAll(o => o.OrderId == order.TransactionId) > 0)
                SavePending();
            OrderFinished?.Invoke(order, true, null);
        }

        /// <summary>Owned products come from the server's entitlements: nothing to restore from the store.</summary>
        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<StoreOrder>>(new List<StoreOrder>());

        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        public virtual void Dispose() { }

        // ---- for derived adapters ----

        /// <summary>Started orders not yet answered by the server.</summary>
        protected List<CheckoutOrder> Pending => m_Pending ?? (m_Pending = LoadPending());

        /// <summary>Order ids waiting for an answer (tests, diagnostics).</summary>
        internal IReadOnlyList<string> PendingOrderIds => Pending.Select(o => o.OrderId).ToList();

        /// <summary>Remembers an order until the server answers for it.</summary>
        protected CheckoutOrder Remember(string orderId, string sku, string userId)
        {
            var existing = Pending.FirstOrDefault(o => o.OrderId == orderId);
            if (existing != null)
                return existing;
            var order = new CheckoutOrder { OrderId = orderId, Sku = sku ?? "", UserId = userId, CreatedTicks = UtcNow().Ticks };
            Pending.Add(order);
            SavePending();
            return order;
        }

        protected void RaisePending(StoreOrder order) => OrderPending?.Invoke(order);

        protected void Fail(string sku, string reason, bool cancelled = false) =>
            PurchaseFailed?.Invoke(new StorePurchaseFailure(sku, $"{Store}: {reason}", cancelled));

        /// <summary>The order as the billing service sees it; <see cref="StoreOrder.StoreUserId"/> is the buyer.</summary>
        protected virtual StoreOrder ToOrder(CheckoutOrder o, object native = null) =>
            new StoreOrder(Platform, o.Sku, o.OrderId, null, native, o.UserId);

        /// <summary>The buyer of an order, or null when another player of this device bought it (the server grants it
        /// to that player; it is not reported to the current one).</summary>
        protected string BuyerOf(StoreOrder order)
        {
            var user = string.IsNullOrEmpty(order.StoreUserId) ? CurrentUser : order.StoreUserId;
            if (string.IsNullOrEmpty(user) || (CurrentUser != null && user != CurrentUser))
                return null;
            return user;
        }

        /// <summary>Asks the server until the order is no longer unpaid or <paramref name="timeout"/> passes (then the
        /// last answer, usually <c>pending_or_canceled</c>). <paramref name="stop"/>: the player gave up.</summary>
        protected async Task<ApiResult<VerifyResponse>> PollAsync(Func<CancellationToken, Task<ApiResult<VerifyResponse>>> ask,
            TimeSpan interval, TimeSpan timeout, Func<bool> stop, CancellationToken ct)
        {
            var deadline = UtcNow() + timeout;
            while (true)
            {
                if (stop != null && stop())
                    return Answer(VerifyStatus.PendingOrCanceled, "checkout cancelled");
                var res = await ask(ct);
                var unpaid = res == null || res.IsTransportError || res.HttpStatus >= 500 ||
                             res.Value?.Status == VerifyStatus.PendingOrCanceled;
                if (!unpaid || UtcNow() >= deadline)
                    return res;
                await Delay(interval, ct);
            }
        }

        protected static ApiResult<VerifyResponse> Answer(string status, string message) =>
            new ApiResult<VerifyResponse>(200, new VerifyResponse { Status = status, Message = message }, null);

        protected static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;

        private List<CheckoutOrder> LoadPending()
        {
            string json = null;
            try
            {
                json = m_Storage.Load(m_PendingKey);
                var list = string.IsNullOrEmpty(json)
                    ? new List<CheckoutOrder>()
                    : JsonConvert.DeserializeObject<List<CheckoutOrder>>(json) ?? new List<CheckoutOrder>();
                list.RemoveAll(o => string.IsNullOrEmpty(o?.OrderId));
                return list;
            }
            catch (Exception e)
            {
                // Keep the unreadable value: the next save would overwrite orders that may be paid.
                Log.Error($"Could not read the started {Store} orders ({e.Message}); kept under {m_PendingKey}.corrupt.");
                try
                {
                    m_Storage.Save(m_PendingKey + ".corrupt", json);
                }
                catch (Exception)
                {
                    // Best effort.
                }
                return new List<CheckoutOrder>();
            }
        }

        private void SavePending()
        {
            try
            {
                m_Storage.Save(m_PendingKey, JsonConvert.SerializeObject(Pending));
            }
            catch (Exception e)
            {
                Log.Warning($"Could not remember the started {Store} orders: {e.Message}");
            }
        }
    }
}
