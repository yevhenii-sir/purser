using System;
using System.Collections.Generic;
using System.Linq;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Player tokens (<c>Authorization: Bearer</c>) and rate limits (429 <c>Retry-After</c>).</summary>
    public sealed class ClientAuthTests
    {
        private static readonly HttpResponse ClientAuthRefused =
            new HttpResponse(401, "{\"status\":\"error\",\"code\":\"ERR_CLIENT_AUTH\",\"message\":\"token expired\"}");

        private static BillingApiClient Api(FakeTransport transport, IPlayerTokenProvider tokens)
        {
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);
            api.SetPlayerTokenProvider(tokens);
            return api;
        }

        [Test]
        public void Requests_CarryTheBearerToken_ReusedUntilRefused()
        {
            var transport = new FakeTransport();
            var tokens = new FakeTokenProvider();
            var api = Api(transport, tokens);

            Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { AppId = "a", UserId = "u1" }));
            Sync.Run(api.VerifyAsync(BillingStores.Google, new VerifyRequest { UserId = "u1" }));
            Sync.Run(api.AttributesAsync(new AttributesRequest { UserId = "u1" }));
            Sync.Run(api.PaywallEventAsync(new PaywallEventRequest { UserId = "u1" }));
            Sync.Run(api.SignApplePromotionalOfferAsync(new ApplePromotionalOfferRequest { UserId = "u1" }));
            Sync.Run(api.CreateStripeCheckoutAsync(new StripeCheckoutRequest { UserId = "u1" }));

            Assert.AreEqual(6, transport.Requests.Count);
            for (var i = 0; i < transport.Requests.Count; i++)
                Assert.AreEqual("Bearer token-1-u1", transport.Authorization(i), transport.Requests[i].Url);
            CollectionAssert.AreEqual(new[] { "u1" }, tokens.Calls, "the token is asked for once");

            Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { UserId = "u2" }));
            Assert.AreEqual("Bearer token-2-u2", transport.Authorization(6), "per player");
        }

        [Test]
        public void ClientAuthRefusal_AsksForANewTokenAndRetriesOnce()
        {
            var transport = new FakeTransport();
            transport.Responses.Enqueue(ClientAuthRefused);
            transport.Responses.Enqueue(new HttpResponse(200, "{\"status\":\"success\"}"));
            var tokens = new FakeTokenProvider();
            var api = Api(transport, tokens);

            var result = Sync.Run(api.VerifyAsync(BillingStores.Google, new VerifyRequest { UserId = "u1" }));

            Assert.AreEqual(VerifyStatus.Success, result.Value.Status);
            Assert.AreEqual(2, transport.Requests.Count);
            Assert.AreEqual("Bearer token-1-u1", transport.Authorization(0));
            Assert.AreEqual("Bearer token-2-u1", transport.Authorization(1));
            Assert.AreEqual(transport.Requests[0].Body, transport.Requests[1].Body, "the same request again");

            Sync.Run(api.VerifyAsync(BillingStores.Google, new VerifyRequest { UserId = "u1" }));
            Assert.AreEqual("Bearer token-2-u1", transport.Authorization(2), "the new token is kept");
            Assert.AreEqual(2, tokens.Calls.Count);
        }

        [Test]
        public void ClientAuthRefusal_IsRetriedOnlyOnce_AndOtherRefusalsNever()
        {
            var transport = new FakeTransport { Next = ClientAuthRefused };
            var tokens = new FakeTokenProvider();
            var api = Api(transport, tokens);

            var refused = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { UserId = "u1" }));
            Assert.AreEqual(2, transport.Requests.Count);
            Assert.AreEqual((401, ErrorCodes.ClientAuth), (refused.HttpStatus, refused.Code));

            transport.Next = new HttpResponse(401, "{\"status\":\"error\",\"code\":\"ERR_SOMETHING_ELSE\"}");
            Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { UserId = "u1" }));
            Assert.AreEqual(3, transport.Requests.Count, "no retry for another 401");
        }

        [Test]
        public void WithoutAPlayerOrAProvider_NoTokenIsSent()
        {
            var transport = new FakeTransport();
            var tokens = new FakeTokenProvider();
            var api = Api(transport, tokens);
            Sync.Run(api.PaywallAsync(new PaywallRequest { AppId = "a", Placement = "main" }));
            Assert.IsNull(transport.Headers[0]);
            Assert.IsEmpty(tokens.Calls);

            var plain = new BillingApiClient("https://b.example.com/route", 10, transport);
            Sync.Run(plain.EntitlementsAsync(new EntitlementsRequest { UserId = "u1" }));
            Assert.IsNull(transport.Authorization(1));
        }

        [Test]
        public void ProviderFailure_IsReportedLikeANetworkError()
        {
            var transport = new FakeTransport();
            var api = Api(transport, new FuncPlayerTokenProvider((user, ct) => throw new InvalidOperationException("backend down")));
            var result = Sync.Run(api.VerifyAsync(BillingStores.Google, new VerifyRequest { UserId = "u1" }));
            Assert.IsTrue(result.IsTransportError, "retried later like a network failure");
            StringAssert.Contains("backend down", result.Error);
            Assert.IsEmpty(transport.Requests);
        }

        [Test]
        public void RateLimitedAnswer_HasRetryAfter_CappedAtAMinute()
        {
            var transport = new FakeTransport
            {
                Next = new HttpResponse(429, "{\"status\":\"error\",\"code\":\"ERR_RATE_LIMITED\",\"message\":\"slow down\"}", null,
                    new Dictionary<string, string> { ["Retry-After"] = "7" }),
            };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);
            var result = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { UserId = "u1" }));
            Assert.IsTrue(result.IsRateLimited);
            Assert.AreEqual(ErrorCodes.RateLimited, result.Code);
            Assert.AreEqual(TimeSpan.FromSeconds(7), result.RetryAfter);

            transport.Next = new HttpResponse(429, "{}", null, new Dictionary<string, string> { ["retry-after"] = "3600" });
            Assert.AreEqual(TimeSpan.FromSeconds(60), Sync.Run(api.EntitlementsAsync(new EntitlementsRequest())).RetryAfter);
            transport.Next = new HttpResponse(429, "{}");
            Assert.IsNull(Sync.Run(api.EntitlementsAsync(new EntitlementsRequest())).RetryAfter);
            transport.Next = new HttpResponse(503, "{}", null, new Dictionary<string, string> { ["Retry-After"] = "5" });
            Assert.IsNull(Sync.Run(api.EntitlementsAsync(new EntitlementsRequest())).RetryAfter, "only for 429");
        }

        [Test]
        public void Service_HandsTheProviderToTheApiAndTheStoreAdapter()
        {
            var config = BillingConfig.Create("https://billing.example.com/route", new BillingProductConfig("gems_100", BillingProductType.Consumable));
            config.WithBuild(BillingStores.Xsolla, "com.test.game.web");
            config.refreshEntitlementsOnInit = false;
            config.logLevel = BillingLogLevel.None;
            var transport = new FakeTransport
            {
                Next = new HttpResponse(400, "{\"status\":\"invalid\",\"message\":\"sandbox off\"}"),
            };
            var xsolla = new XsollaStoreAdapter(config, new XsollaOptions { OpenUrl = _ => { } }, transport, new MemoryBillingStorage());
            var api = new FakeApi();
            var tokens = new FakeTokenProvider();
            var service = new BillingService(config, new BillingDependencies
            {
                Store = xsolla,
                Api = api,
                UserIdProvider = new StaticUserIdProvider("u1"),
                PlayerTokenProvider = tokens,
                Storage = new MemoryBillingStorage(),
                IsEditor = false,
                CreateLifecycleHook = false,
            });
            try
            {
                Sync.Run(service.InitializeAsync());
                Assert.IsNotNull(api.PlayerTokens);

                var result = Sync.Run(service.PurchaseAsync("gems_100"));

                Assert.AreEqual(PurchaseStatus.Failed, result.Status);
                Assert.AreEqual("https://billing.example.com/route/checkout/xsolla", transport.Requests.Single().Url);
                Assert.AreEqual("Bearer token-1-u1", transport.Authorization(0));
                // The API client and the adapter share one cache: the game backend is asked once.
                Assert.AreEqual("token-1-u1", Sync.Run(api.PlayerTokens.GetTokenAsync("u1", default)));
                Assert.AreEqual(1, tokens.Calls.Count);
            }
            finally
            {
                service.Dispose();
                UnityEngine.Object.DestroyImmediate(config);
            }
        }

        [Test]
        public void Stripe_SignsItsRoutesThroughTheApiClient()
        {
            var config = BillingConfig.Create("https://billing.example.com/route", new BillingProductConfig("gems_100", BillingProductType.Consumable));
            try
            {
                var transport = new FakeTransport { Next = new HttpResponse(500, "{}") };
                var stripe = new StripeStoreAdapter(config,
                    new StripeCheckoutOptions { SuccessUrl = "https://example.com/ok", CancelUrl = "https://example.com/back", OpenUrl = _ => { } },
                    new BillingApiClient(config, transport), new MemoryBillingStorage());
                stripe.SetPlayerTokenProvider(new FakeTokenProvider());
                stripe.PrepareCheckout(new CheckoutContext { UserId = "u1" });
                stripe.Purchase("gems_100");
                Assert.AreEqual("Bearer token-1-u1", transport.Authorization(0));
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(config);
            }
        }

        [Test]
        public void Verification_WaitsForRetryAfter_AndARateLimitDoesNotFinishThePurchase()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.VerifyResults.Enqueue(FakeApi.RateLimited(7));
                h.Api.VerifyResults.Enqueue(FakeApi.RateLimited(45));
                h.Api.VerifyResults.Enqueue(FakeApi.Verify(200, VerifyStatus.Success));
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);

                var result = Sync.Run(h.Service.PurchaseAsync("gems_100"));

                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status, "the caller is answered after the first try");
                CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(7), TimeSpan.FromSeconds(45) }, h.Delays,
                    "Retry-After instead of retryDelaysSeconds");
                Assert.AreEqual(3, h.Api.VerifyRequests.Count);
                Assert.AreEqual(1, h.Store.Confirmed.Count);
                Assert.AreEqual(1, h.Succeeded.Count);
                Assert.IsEmpty(h.Failed);
            }
        }

        [Test]
        public void Verification_RateLimitedForever_GivesUpWithoutFinishing()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.RateLimited(1);
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);

                Sync.Run(h.Service.PurchaseAsync("gems_100"));

                Assert.AreEqual(BillingService.MaxRateLimitedRetries + h.Config.retryDelaysSeconds.Length + 1, h.Api.VerifyRequests.Count);
                Assert.IsEmpty(h.Store.Confirmed, "the store delivers it again on the next launch");
                Assert.IsEmpty(h.Failed);
            }
        }

        [Test]
        public void Paywall_RateLimited_UsesTheCachedAnswer()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.PaywallResult = FakeApi.Paywall("{\"status\":\"ok\",\"placement\":\"main\",\"paywall_id\":\"pw\",\"variant_id\":\"a\"," +
                                                      "\"products\":[{\"id\":\"gems_100\",\"store_id\":\"gems_100\",\"kind\":\"consumable\"}]}");
                Assert.IsFalse(Sync.Run(h.Service.GetPaywallAsync("main")).IsFromCache);

                h.Api.PaywallResult = new ApiResult<PaywallResponse>(429, null, "HTTP 429", ErrorCodes.RateLimited, TimeSpan.FromSeconds(5));
                var cached = Sync.Run(h.Service.GetPaywallAsync("main"));
                Assert.IsTrue(cached.IsFromCache);
                Assert.AreEqual("a", cached.VariantId);
            }
        }

        [Test]
        public void VerifyRequest_SendsOfferId_OnlyWhenSet()
        {
            var json = JObject.Parse(Newtonsoft.Json.JsonConvert.SerializeObject(new VerifyRequest { AppId = "a", OfferId = "intro-7d" }));
            Assert.AreEqual("intro-7d", (string)json["offer_id"]);
            Assert.IsFalse(JObject.Parse(Newtonsoft.Json.JsonConvert.SerializeObject(new VerifyRequest())).ContainsKey("offer_id"));
        }
    }
}
