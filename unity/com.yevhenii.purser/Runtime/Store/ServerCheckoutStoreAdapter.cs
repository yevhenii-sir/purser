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
        /// <summary>The player of the next <see cref="IStoreAdapter.Purchase"/>.</summary>
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
    /// binding (the server puts the billing user id into the order). Derive from it for another such store; its
    /// server calls sign with the player tokens of <see cref="PlayerTokens"/>.
    /// </summary>
    public abstract class ServerCheckoutStoreAdapter : ICheckoutStoreAdapter, IPlayerTokenConsumer
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
        /// <summary>Cancelled, never disposed: callbacks arriving after <see cref="Dispose"/> may still read the token.</summary>
        private readonly CancellationTokenSource m_Lifetime = new CancellationTokenSource();
        private List<CheckoutOrder> m_Pending;
        /// <summary>Orders raised again (<see cref="RaiseResumed"/>), not started by a checkout of this run.</summary>
        private readonly HashSet<string> m_Resumed = new HashSet<string>();

        /// <summary>How many times the server is asked about a resumed order (an earlier run's, or one the player comes
        /// back to) before it is reported as not paid yet: nobody is paying right now, unlike after a checkout.</summary>
        protected const int ResumeAttempts = 3;

        /// <summary>Requests in a row that may fail (no network, 5xx) before a check gives its last answer back: the
        /// billing service retries on its own schedule instead of this adapter asking every few seconds.</summary>
        protected const int MaxFailedRequests = 3;

        /// <summary>Test hook: waits between polls.</summary>
        internal Func<TimeSpan, CancellationToken, Task> Delay = (d, ct) => Task.Delay(d, ct);

        /// <summary>Test hook: the clock.</summary>
        internal Func<DateTime> UtcNow = () => DateTime.UtcNow;

        /// <summary>The adapter's log.</summary>
        protected BillingLog Log { get; }

        /// <summary>Cancelled by <see cref="Dispose"/>: pass it to server calls and waits of the adapter.</summary>
        protected CancellationToken Lifetime => m_Lifetime.Token;

        /// <summary><see cref="Dispose"/> was called.</summary>
        protected bool IsDisposed { get; private set; }

        /// <summary>The player of the purchase being started (<see cref="PrepareCheckout"/>).</summary>
        protected CheckoutContext Checkout { get; private set; }

        /// <summary>Billing user id of the purchase being started; null before the first <see cref="PrepareCheckout"/>.</summary>
        protected string CurrentUser => string.IsNullOrEmpty(Checkout?.UserId) ? null : Checkout.UserId;

        /// <summary>Player tokens for the adapter's server calls (<see cref="IPlayerTokenProvider"/>); null when the build
        /// does not sign client requests.</summary>
        protected IPlayerTokenProvider PlayerTokens { get; private set; }

        /// <summary>Signs the adapter's server calls with player tokens. Overrides call the base.</summary>
        public virtual void SetPlayerTokenProvider(IPlayerTokenProvider provider) => PlayerTokens = provider;

        /// <inheritdoc/>
        public abstract BillingPlatform Platform { get; }
        /// <inheritdoc/>
        public abstract string Store { get; }

        /// <inheritdoc/>
        public event Action<StoreOrder> OrderPending;
        /// <summary>Not used: an unpaid order is reported by the order check.</summary>
        public event Action<StoreOrder> OrderDeferred { add { } remove { } }
        /// <inheritdoc/>
        public event Action<StorePurchaseFailure> PurchaseFailed;
        /// <inheritdoc/>
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
                RaiseResumed(ToOrder(o));
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

        /// <inheritdoc/>
        public void PrepareCheckout(CheckoutContext context) => Checkout = context;

        /// <summary>Starts the checkout (<see cref="StartCheckoutAsync"/>); the result arrives through the events.</summary>
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
            catch (OperationCanceledException) when (IsDisposed)
            {
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

        /// <summary>Nothing to restore from the store.</summary>
        public Task<bool> RestoreTransactionsAsync(CancellationToken ct) => Task.FromResult(true);

        /// <summary>Cancels running checkouts and polls (<see cref="Lifetime"/>). Overrides call the base.</summary>
        public virtual void Dispose()
        {
            if (IsDisposed)
                return;
            IsDisposed = true;
            m_Lifetime.Cancel();
        }

        // ---- for derived adapters ----

        /// <summary>Started orders not yet answered by the server.</summary>
        protected List<CheckoutOrder> Pending => m_Pending ?? (m_Pending = LoadOrders(m_PendingKey));

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

        /// <summary>Raises <see cref="OrderPending"/> for an order whose checkout just started: the billing service
        /// checks it with the server until it is paid (<see cref="CheckAsync"/> polls for the whole payment timeout).</summary>
        protected void RaisePending(StoreOrder order)
        {
            if (!string.IsNullOrEmpty(order?.TransactionId))
                m_Resumed.Remove(order.TransactionId);
            OrderPending?.Invoke(order);
        }

        /// <summary>Raises <see cref="OrderPending"/> for an order that was started earlier (a previous run, or a page
        /// the player comes back from): <see cref="CheckAsync"/> asks about it a few times only.</summary>
        protected void RaiseResumed(StoreOrder order)
        {
            if (!string.IsNullOrEmpty(order?.TransactionId))
                m_Resumed.Add(order.TransactionId);
            OrderPending?.Invoke(order);
        }

        /// <summary>Whether the payment page the server returned may be opened: only an absolute https URL. Otherwise
        /// the purchase of <paramref name="sku"/> fails.</summary>
        protected bool IsSafePage(string sku, string url)
        {
            if (UrlPolicy.IsHttps(url))
                return true;
            Log.Warning($"{Store} checkout: the server returned a payment page that is not an https URL; it is not opened.");
            Fail(sku, "checkout: the payment page is not an https URL");
            return false;
        }

        /// <summary>Raises <see cref="PurchaseFailed"/> for the purchase of <paramref name="sku"/>.</summary>
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

        /// <summary>
        /// The server's answer for <paramref name="order"/> (<see cref="IWebhookStoreAdapter.CheckOrderAsync"/>). An
        /// order whose checkout just started is polled until it is paid or <paramref name="timeout"/> passes
        /// (<see cref="PollAsync"/>). A resumed one (<see cref="RaiseResumed"/>) is asked <see cref="ResumeAttempts"/>
        /// times, waiting <paramref name="interval"/>, then twice as long: it is paid already or not being paid now.
        /// </summary>
        protected Task<ApiResult<VerifyResponse>> CheckAsync(StoreOrder order, Func<CancellationToken, Task<ApiResult<VerifyResponse>>> ask,
            TimeSpan interval, TimeSpan timeout, Func<bool> stop, CancellationToken ct)
        {
            var resumed = !string.IsNullOrEmpty(order?.TransactionId) && m_Resumed.Contains(order.TransactionId);
            return PollAsync(ask, interval, timeout, stop, resumed ? ResumeAttempts : 0, ct);
        }

        /// <summary>Asks the server until the order is no longer unpaid or <paramref name="timeout"/> passes (then the
        /// last answer, usually <c>pending_or_canceled</c>). <paramref name="stop"/>: the player gave up. A rate-limited
        /// answer (429) waits for its <c>Retry-After</c> when that is longer than <paramref name="interval"/>.
        /// <see cref="MaxFailedRequests"/> failed requests in a row end it with the last one (a transport error, which
        /// the billing service retries later).</summary>
        protected Task<ApiResult<VerifyResponse>> PollAsync(Func<CancellationToken, Task<ApiResult<VerifyResponse>>> ask,
            TimeSpan interval, TimeSpan timeout, Func<bool> stop, CancellationToken ct) =>
            PollAsync(ask, interval, timeout, stop, 0, ct);

        /// <summary>Ends the waits of the orders being polled (<see cref="CheckNow"/>).</summary>
        private TaskCompletionSource<bool> m_Wake = new TaskCompletionSource<bool>();

        /// <summary>The player came back from the payment page (e.g. its app link opened the game): the orders being
        /// checked are asked about now instead of after the poll interval.</summary>
        public void CheckNow()
        {
            var wake = m_Wake;
            m_Wake = new TaskCompletionSource<bool>();
            wake.TrySetResult(true);
        }

        /// <param name="maxAttempts">Above 0: at most this many requests, each wait twice the previous one.</param>
        private async Task<ApiResult<VerifyResponse>> PollAsync(Func<CancellationToken, Task<ApiResult<VerifyResponse>>> ask,
            TimeSpan interval, TimeSpan timeout, Func<bool> stop, int maxAttempts, CancellationToken ct)
        {
            var deadline = UtcNow() + timeout;
            var failures = 0;
            for (var attempt = 1; ; attempt++)
            {
                if (stop != null && stop())
                    return Answer(VerifyStatus.PendingOrCanceled, "checkout cancelled");
                var res = await ask(ct);
                var failed = res == null || res.IsTransportError || res.HttpStatus >= 500;
                failures = failed ? failures + 1 : 0;
                var unpaid = failed || res.IsRateLimited || res.Value?.Status == VerifyStatus.PendingOrCanceled;
                if (!unpaid || UtcNow() >= deadline || failures >= MaxFailedRequests || (maxAttempts > 0 && attempt >= maxAttempts))
                    return res;
                var wait = maxAttempts > 0 ? TimeSpan.FromTicks(interval.Ticks << (attempt - 1)) : interval;
                if (res?.RetryAfter is TimeSpan retryAfter && retryAfter > wait)
                    wait = retryAfter;
                var wake = m_Wake.Task;
                using (var cts = CancellationTokenSource.CreateLinkedTokenSource(ct))
                {
                    await Task.WhenAny(Delay(wait, cts.Token), wake);
                    cts.Cancel();
                }
                ct.ThrowIfCancellationRequested();
            }
        }

        /// <summary>A server-like answer made up by the adapter (e.g. the player gave up).</summary>
        protected static ApiResult<VerifyResponse> Answer(string status, string message) =>
            new ApiResult<VerifyResponse>(200, new VerifyResponse { Status = status, Message = message }, null);

        /// <summary>Null for an empty string (optional JSON fields are left out).</summary>
        protected static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;

        /// <summary>Orders saved under <paramref name="key"/>; an unreadable value is kept under <c>{key}.corrupt</c>.</summary>
        private protected List<CheckoutOrder> LoadOrders(string key)
        {
            string json = null;
            try
            {
                json = m_Storage.Load(key);
                var list = string.IsNullOrEmpty(json)
                    ? new List<CheckoutOrder>()
                    : JsonConvert.DeserializeObject<List<CheckoutOrder>>(json) ?? new List<CheckoutOrder>();
                list.RemoveAll(o => string.IsNullOrEmpty(o?.OrderId));
                return list;
            }
            catch (Exception e)
            {
                // Keep the unreadable value: the next save would overwrite orders that may be paid.
                Log.Error($"Could not read the started {Store} orders ({e.Message}); kept under {key}.corrupt.");
                try
                {
                    m_Storage.Save(key + ".corrupt", json);
                }
                catch (Exception copy)
                {
                    Log.Warning($"Could not keep the unreadable {Store} orders: {copy.Message}");
                }
                return new List<CheckoutOrder>();
            }
        }

        private void SavePending() => SaveOrders(m_PendingKey, Pending);

        /// <summary>Saves <paramref name="orders"/> under <paramref name="key"/>; a failure is logged.</summary>
        private protected void SaveOrders(string key, List<CheckoutOrder> orders)
        {
            try
            {
                m_Storage.Save(key, JsonConvert.SerializeObject(orders));
                // A started order that is forgotten cannot be checked again: on disk at once.
                (m_Storage as IFlushableBillingStorage)?.Flush();
            }
            catch (Exception e)
            {
                Log.Warning($"Could not remember the started {Store} orders: {e.Message}");
            }
        }
    }
}
