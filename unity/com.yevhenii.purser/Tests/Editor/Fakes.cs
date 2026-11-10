using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    internal sealed partial class FakeStore : IStoreAdapter
    {
        public BillingPlatform Platform { get; set; } = BillingPlatform.Google;
        private string m_Store;
        /// <summary>The platform's store unless set (web stores).</summary>
        public string Store { get => m_Store ?? BillingStores.Of(Platform) ?? BillingStores.Google; set => m_Store = value; }

        public event Action<StoreOrder> OrderPending;
        public event Action<StoreOrder> OrderDeferred;
        public event Action<StorePurchaseFailure> PurchaseFailed;
        public event Action<StoreOrder, bool, string> OrderFinished;

        public readonly List<string> PurchaseCalls = new List<string>();
        public readonly List<StoreOrder> Confirmed = new List<StoreOrder>();
        /// <summary>The <c>granted</c> flag of every <see cref="Finish"/>, in order.</summary>
        public readonly List<bool> FinishedGranted = new List<bool>();
        public readonly List<StoreOrder> Owned = new List<StoreOrder>();
        public string BoundUserId;
        public bool RestoreCalled;
        /// <summary>The store reports no prices (like Steam or a web store).</summary>
        public bool NoPrices;
        public bool Disposed;

        /// <summary>What happens on Purchase(storeId): return an order to deliver as pending, or null.</summary>
        public Func<string, StoreOrder> OnPurchase;

        public Task ConnectAsync(CancellationToken ct) => Task.CompletedTask;

        public Task<IReadOnlyList<StoreProduct>> FetchProductsAsync(IReadOnlyList<BillingProductConfig> products, CancellationToken ct)
        {
            IReadOnlyList<StoreProduct> list = products
                .Select(p => NoPrices
                    ? new StoreProduct(p.id, p.StoreId(Store), p.type, "", 0m, null, p.id, "", true)
                    : new StoreProduct(p.id, p.StoreId(Store), p.type, "$0.99", 0.99m, "USD", p.id, "", true))
                .ToList();
            return Task.FromResult(list);
        }

        public void SetAccountBinding(string userId) => BoundUserId = userId;

        public void Purchase(string storeProductId)
        {
            PurchaseCalls.Add(storeProductId);
            var order = OnPurchase?.Invoke(storeProductId);
            if (order != null)
                OrderPending?.Invoke(order);
        }

        public void Deliver(StoreOrder order) => OrderPending?.Invoke(order);
        public void Defer(StoreOrder order) => OrderDeferred?.Invoke(order);
        public void Fail(string storeId, bool cancelled) => PurchaseFailed?.Invoke(new StorePurchaseFailure(storeId, "test failure", cancelled));

        public void Finish(StoreOrder order, bool granted)
        {
            Confirmed.Add(order);
            FinishedGranted.Add(granted);
            OrderFinished?.Invoke(order, true, null);
        }

        public Task<IReadOnlyList<StoreOrder>> FetchPurchasesAsync(CancellationToken ct) =>
            Task.FromResult<IReadOnlyList<StoreOrder>>(Owned.ToList());

        public Task<bool> RestoreTransactionsAsync(CancellationToken ct)
        {
            RestoreCalled = true;
            return Task.FromResult(true);
        }

        public void Dispose() => Disposed = true;

        public static StoreOrder GoogleOrder(string storeId, string token = "token-1", string orderId = "GPA.1") =>
            new StoreOrder(BillingPlatform.Google, storeId, orderId, token);

        public static StoreOrder AppleOrder(string storeId, string transactionId = "2000000001") =>
            new StoreOrder(BillingPlatform.Apple, storeId, transactionId, null);
    }

    internal sealed partial class FakeApi : IBillingApi
    {
        public readonly List<VerifyRequest> VerifyRequests = new List<VerifyRequest>();
        public readonly List<string> VerifyStores = new List<string>();
        public readonly Queue<ApiResult<VerifyResponse>> VerifyResults = new Queue<ApiResult<VerifyResponse>>();
        public ApiResult<VerifyResponse> DefaultVerify = Verify(200, VerifyStatus.Success);

        /// <summary>When set, the next verify call waits for this task (simulates a slow server).</summary>
        public TaskCompletionSource<ApiResult<VerifyResponse>> Hold;

        public readonly List<RestoreRequest> RestoreRequests = new List<RestoreRequest>();
        public ApiResult<RestoreResponse> RestoreResult;

        public int EntitlementsCalls;
        public ApiResult<EntitlementsDto> EntitlementsResult = new ApiResult<EntitlementsDto>(500, null, "not configured");

        public Task<ApiResult<VerifyResponse>> VerifyAsync(string store, VerifyRequest request, CancellationToken ct = default)
        {
            VerifyRequests.Add(request);
            VerifyStores.Add(store);
            if (Hold != null)
            {
                var hold = Hold;
                Hold = null;
                return hold.Task;
            }
            return Task.FromResult(VerifyResults.Count > 0 ? VerifyResults.Dequeue() : DefaultVerify);
        }

        public Task<ApiResult<RestoreResponse>> RestoreAsync(RestoreRequest request, CancellationToken ct = default)
        {
            RestoreRequests.Add(request);
            return Task.FromResult(RestoreResult ?? new ApiResult<RestoreResponse>(500, null, "not configured"));
        }

        public Task<ApiResult<EntitlementsDto>> EntitlementsAsync(EntitlementsRequest request, CancellationToken ct = default)
        {
            EntitlementsCalls++;
            return Task.FromResult(EntitlementsResult);
        }

        public Task<ApiResult<string>> HealthAsync(CancellationToken ct = default) =>
            Task.FromResult(new ApiResult<string>(200, "{\"status\":\"healthy\"}", null));

        public readonly List<PaywallRequest> PaywallRequests = new List<PaywallRequest>();
        public ApiResult<PaywallResponse> PaywallResult = new ApiResult<PaywallResponse>(500, null, "not configured");
        public readonly List<PaywallEventRequest> PaywallEvents = new List<PaywallEventRequest>();
        public ApiResult<StatusResponse> PaywallEventResult = new ApiResult<StatusResponse>(200, new StatusResponse { Status = "ok" }, null);

        public Task<ApiResult<PaywallResponse>> PaywallAsync(PaywallRequest request, CancellationToken ct = default)
        {
            PaywallRequests.Add(request);
            return Task.FromResult(PaywallResult);
        }

        public Task<ApiResult<StatusResponse>> PaywallEventAsync(PaywallEventRequest request, CancellationToken ct = default)
        {
            PaywallEvents.Add(request);
            return Task.FromResult(PaywallEventResult);
        }

        /// <summary>A `/paywall` answer as the server sends it.</summary>
        public static ApiResult<PaywallResponse> Paywall(string json)
        {
            var value = Newtonsoft.Json.JsonConvert.DeserializeObject<PaywallResponse>(json);
            return new ApiResult<PaywallResponse>(200, value, null);
        }

        public static ApiResult<VerifyResponse> Verify(int http, string status, string message = null, string delivery = null) =>
            new ApiResult<VerifyResponse>(http,
                new VerifyResponse { Status = status, Message = message, TransactionId = "GPA.1", Delivery = delivery },
                http >= 200 && http < 300 ? null : "HTTP " + http);

        public static ApiResult<VerifyResponse> TransportError() => new ApiResult<VerifyResponse>(0, null, "no network");

        public static ApiResult<EntitlementsDto> Entitlements(Dictionary<string, long> subs, params string[] nonConsumables) =>
            new ApiResult<EntitlementsDto>(200, new EntitlementsDto
            {
                Subscriptions = subs,
                NonConsumables = nonConsumables.ToList(),
                ServerTimeMs = 1_800_000_000_000,
            }, null);
    }

    internal sealed class FakeTransport : IHttpTransport
    {
        public readonly List<(string Method, string Url, string Body)> Requests = new List<(string, string, string)>();
        public HttpResponse Next = new HttpResponse(200, "{}");

        public Task<HttpResponse> SendAsync(string method, string url, string jsonBody, int timeoutSeconds, CancellationToken ct)
        {
            Requests.Add((method, url, jsonBody));
            return Task.FromResult(Next);
        }
    }

    internal static class Sync
    {
        /// <summary>With fakes everything completes synchronously; fail loudly if not.</summary>
        public static T Run<T>(Task<T> task)
        {
            Assert.IsTrue(task.IsCompleted, "task was expected to complete synchronously");
            return task.GetAwaiter().GetResult();
        }

        public static void Run(Task task)
        {
            Assert.IsTrue(task.IsCompleted, "task was expected to complete synchronously");
            task.GetAwaiter().GetResult();
        }
    }

    // ---- App Store promotional offers ----

    internal sealed partial class FakeStore : IApplePromotionalOfferStore
    {
        public readonly List<(string StoreId, ApplePromotionalOffer Offer)> OfferPurchases =
            new List<(string, ApplePromotionalOffer)>();

        public void PurchaseWithPromotionalOffer(string storeProductId, ApplePromotionalOffer offer)
        {
            OfferPurchases.Add((storeProductId, offer));
            var order = OnPurchase?.Invoke(storeProductId);
            if (order != null)
                OrderPending?.Invoke(order);
        }
    }

    internal sealed partial class FakeApi : IApplePromotionalOfferApi
    {
        public readonly List<ApplePromotionalOfferRequest> OfferRequests = new List<ApplePromotionalOfferRequest>();

        /// <summary>Answer of the next signature request; default: a valid signature for the requested offer.</summary>
        public Func<ApplePromotionalOfferRequest, ApiResult<ApplePromotionalOfferResponse>> OfferResult = SignedOffer;

        public Task<ApiResult<ApplePromotionalOfferResponse>> SignApplePromotionalOfferAsync(ApplePromotionalOfferRequest request,
            CancellationToken ct = default)
        {
            OfferRequests.Add(request);
            return Task.FromResult(OfferResult(request));
        }

        public static ApiResult<ApplePromotionalOfferResponse> SignedOffer(ApplePromotionalOfferRequest request) =>
            new ApiResult<ApplePromotionalOfferResponse>(200, new ApplePromotionalOfferResponse
            {
                Status = VerifyStatus.Success,
                ProductId = request.ProductId,
                OfferId = request.OfferId,
                KeyId = "KEY123ABCD",
                Nonce = "a1b2c3d4-0000-4000-8000-00000000000f",
                Timestamp = 1_800_000_000_000,
                Signature = "MEUCIQD0ZXN0",
                AppAccountToken = AccountIds.AppleAccountToken(request.UserId).ToString(),
                SignedOfferJws = "eyJhbGciOiJFUzI1NiJ9.e30.sig",
                ExpiresAtMs = 1_800_086_400_000,
            }, null);
    }
}
