using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Purchases nobody listens to, failed initialization, hung providers, given-up orders, restore and
    /// dispose: what must not be lost, repeated or left waiting.</summary>
    public sealed class ReliabilityTests
    {
        private static readonly ApiResult<VerifyResponse> ClientGrant =
            FakeApi.Verify(200, VerifyStatus.Success, delivery: VerifyDelivery.Client);

        private static Paywall Paywall() =>
            new Paywall("main_paywall", "pw_1", "Main", "variant_b", "test_7", null, null, "{}", false,
                new DateTime(2027, 1, 1, 0, 0, 0, DateTimeKind.Utc));

        /// <summary>Delays that end when the test says so (in the order they were started).</summary>
        private sealed class Gates
        {
            public readonly Queue<TaskCompletionSource<bool>> Open = new Queue<TaskCompletionSource<bool>>();

            public Task Delay(TimeSpan delay, CancellationToken ct)
            {
                var gate = new TaskCompletionSource<bool>();
                Open.Enqueue(gate);
                return gate.Task;
            }

            public void Elapse() => Open.Dequeue().SetResult(true);
        }

        private static ServiceHarness WithGates(out Gates gates)
        {
            var g = gates = new Gates();
            return new ServiceHarness { Delay = g.Delay };
        }

        // ---- Purchases the game grants itself ----

        [Test]
        public void ClientGrant_WithoutAReceiver_IsFinishedOnlyWhenAHandlerTakesIt()
        {
            using (var h = new ServiceHarness { Subscribe = false })
            {
                h.Start();
                h.Api.DefaultVerify = ClientGrant;
                var order = FakeStore.GoogleOrder("gems_100");

                h.Store.Deliver(order);

                Assert.AreEqual(1, h.Api.VerifyRequests.Count);
                Assert.IsEmpty(h.Store.Confirmed, "nobody granted it: the store must be able to deliver it again");

                // Delivered again (a restore, another key of the same transaction): verified already.
                h.Store.Deliver(order);
                h.Store.Deliver(new StoreOrder(BillingPlatform.Google, "gems_100", "GPA.1", "another-token"));
                Assert.AreEqual(1, h.Api.VerifyRequests.Count);
                Assert.IsEmpty(h.Store.Confirmed);

                var first = new List<PurchaseResult>();
                h.Service.PurchaseSucceeded += first.Add;
                Assert.AreEqual(PurchaseStatus.Success, first.Single().Status);
                Assert.IsTrue(first.Single().GrantOnClient);
                Assert.AreEqual(1, h.Store.Confirmed.Count, "finished once the game could grant it");
                Assert.IsTrue(h.Store.FinishedGranted.Single());

                var second = new List<PurchaseResult>();
                h.Service.PurchaseSucceeded += second.Add;
                Assert.IsEmpty(second, "delivered once");
                Assert.AreEqual(1, h.Store.Confirmed.Count);

                // Finished: the store may deliver it again and it is verified like any order.
                h.Store.Deliver(order);
                Assert.AreEqual(2, h.Api.VerifyRequests.Count);
                Assert.AreEqual((2, 1), (first.Count, second.Count), "both handlers got the new delivery");
                Assert.AreEqual(2, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void ClientGrant_StaysHeld_WhenTheHandlerThrows()
        {
            using (var h = new ServiceHarness { Subscribe = false })
            {
                h.Start();
                h.Api.DefaultVerify = ClientGrant;
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));

                var calls = 0;
                h.Service.PurchaseSucceeded += _ =>
                {
                    calls++;
                    throw new InvalidOperationException("the game could not grant it");
                };
                Assert.AreEqual(1, calls);
                Assert.IsEmpty(h.Store.Confirmed, "not granted: not finished");

                var granted = new List<PurchaseResult>();
                h.Service.PurchaseSucceeded += granted.Add;
                Assert.AreEqual(1, granted.Count, "the next handler gets it");
                Assert.AreEqual(1, calls, "only the handler that was added is called");
                Assert.AreEqual(1, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void ClientGrant_AHandlerThatAddsAnotherHandler_DoesNotDeliverTwice()
        {
            using (var h = new ServiceHarness { Subscribe = false })
            {
                h.Start();
                h.Api.DefaultVerify = ClientGrant;
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));

                var inner = new List<PurchaseResult>();
                var outer = new List<PurchaseResult>();
                h.Service.PurchaseSucceeded += r =>
                {
                    outer.Add(r);
                    h.Service.PurchaseSucceeded += inner.Add;
                };

                Assert.AreEqual(1, outer.Count);
                Assert.IsEmpty(inner);
                Assert.AreEqual(1, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void ClientGrant_AwaitedPurchaseWithoutSubscribers_IsFinishedAfterTheCallersCode()
        {
            using (var h = new ServiceHarness { Subscribe = false })
            using (var main = MainThreadQueue.Install())
            {
                h.Start();
                h.Api.DefaultVerify = ClientGrant;
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);

                var result = Sync.Run(h.Service.PurchaseAsync("gems_100"));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                Assert.IsTrue(result.GrantOnClient, "the caller grants it from the result");
                Assert.IsEmpty(h.Store.Confirmed, "not finished before the caller's code ran");
                main.RunAll();
                Assert.AreEqual(1, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void ClientGrant_ThePurchaseIsFinishedOnlyAfterTheCodeAfterTheAwaitGranted()
        {
            using (var h = new ServiceHarness { Subscribe = false })
            using (var main = MainThreadQueue.Install())
            {
                h.Start();
                h.Api.DefaultVerify = ClientGrant;
                var order = FakeStore.GoogleOrder("gems_100");
                var confirmedWhenGranted = -1;
                async Task BuyAndGrant()
                {
                    var r = await h.Service.PurchaseAsync("gems_100");
                    if (r.GrantOnClient)
                        confirmedWhenGranted = h.Store.Confirmed.Count;
                }

                var buying = BuyAndGrant();
                Assert.IsFalse(buying.IsCompleted, "the store has not delivered yet");
                h.Store.Deliver(order);
                main.RunAll();

                Assert.IsTrue(buying.IsCompleted);
                Assert.AreEqual(0, confirmedWhenGranted, "the reward is granted before the store purchase is consumed");
                Assert.AreEqual(1, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void ClientGrant_WithAHandler_IsFinishedRightAfterIt()
        {
            using (var h = new ServiceHarness { Subscribe = false })
            using (MainThreadQueue.Install())
            {
                h.Start();
                h.Api.DefaultVerify = ClientGrant;
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);
                h.Service.PurchaseSucceeded += _ => { };

                Sync.Run(h.Service.PurchaseAsync("gems_100"));

                Assert.AreEqual(1, h.Store.Confirmed.Count, "the handler granted it");
            }
        }

        // ---- Initialization ----

        [Test]
        public void Initialize_WhenTheStoreFails_FaultsTheTask_ThenRetriesWithBackoff()
        {
            using (var h = WithGates(out var gates))
            {
                h.Start(initialize: false);
                var initialized = 0;
                h.Service.Initialized += () => initialized++;
                h.Store.FailConnects = 2;

                var first = h.Service.InitializeAsync();

                Assert.IsTrue(first.IsFaulted, "the caller learns that it failed");
                Assert.IsInstanceOf<BillingException>(first.Exception.GetBaseException());
                Assert.IsFalse(h.Service.IsInitialized);
                CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(5) }, h.Delays, "a retry is waiting");

                gates.Elapse();
                Assert.AreEqual(2, h.Store.ConnectCalls);
                Assert.IsFalse(h.Service.IsInitialized, "the second attempt failed too");
                CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(5), TimeSpan.FromSeconds(10) }, h.Delays);

                gates.Elapse();
                Assert.IsTrue(h.Service.IsInitialized);
                Assert.AreEqual(1, initialized);
                Assert.AreEqual(4, h.Service.Products.Count);
                Sync.Run(h.Service.InitializeAsync());
                Assert.AreEqual(3, h.Store.ConnectCalls, "initialized: nothing is connected again");
                Assert.AreEqual(2, h.Delays.Count, "the retries stopped");
                Assert.AreEqual(PurchaseStatus.Failed, Sync.Run(h.Service.PurchaseAsync("nope")).Status, "purchases work");
            }
        }

        [Test]
        public void Initialize_Retry_DoesNotWaitWhenTheAppReturnsToTheForeground()
        {
            using (var h = WithGates(out var gates))
            {
                h.Start(initialize: false);
                h.Store.FailConnects = 1;
                Assert.IsTrue(h.Service.InitializeAsync().IsFaulted);
                Assert.AreEqual(1, gates.Open.Count);

                h.Service.OnAppResumed();

                Assert.IsTrue(h.Service.IsInitialized, "retried without waiting for the backoff");
                Assert.AreEqual(2, h.Store.ConnectCalls);
            }
        }

        [Test]
        public void Initialize_CalledAgainWhileRetrying_IsOneAttemptAtATime()
        {
            using (var h = WithGates(out var gates))
            {
                h.Start(initialize: false);
                var initialized = 0;
                h.Service.Initialized += () => initialized++;
                h.Store.FailConnects = 1;
                Assert.IsTrue(h.Service.InitializeAsync().IsFaulted);

                Sync.Run(h.Service.InitializeAsync());
                Assert.IsTrue(h.Service.IsInitialized);
                Assert.AreEqual(2, h.Store.ConnectCalls);

                gates.Elapse();
                Assert.AreEqual(2, h.Store.ConnectCalls, "the background retry sees that it is done");
                Assert.AreEqual(1, initialized);
                Assert.IsEmpty(gates.Open);
            }
        }

        [Test]
        public void Initialize_IsNotRetried_AfterDispose()
        {
            using (var h = WithGates(out var gates))
            {
                h.Start(initialize: false);
                h.Store.FailConnects = 5;
                Assert.IsTrue(h.Service.InitializeAsync().IsFaulted);

                h.Service.Dispose();
                while (gates.Open.Count > 0)
                    gates.Elapse();

                Assert.AreEqual(1, h.Store.ConnectCalls);
                Assert.IsTrue(h.Service.InitializeAsync().IsFaulted);
                Assert.IsInstanceOf<ObjectDisposedException>(h.Service.InitializeAsync().Exception.GetBaseException());
            }
        }

        [Test]
        public void Initialize_AConfigurationProblem_IsNotRetried()
        {
            using (var h = WithGates(out var gates))
            {
                h.Config.serverBaseUrl = "http://billing.example.com/route";
                h.Start(initialize: false);

                var first = h.Service.InitializeAsync();

                Assert.IsInstanceOf<BillingException>(first.Exception.GetBaseException());
                StringAssert.Contains("https", first.Exception.GetBaseException().Message);
                Assert.AreEqual(0, h.Store.ConnectCalls);
                Assert.IsEmpty(gates.Open, "no retry is waiting");
                h.Service.OnAppResumed();
                Assert.AreEqual(0, h.Store.ConnectCalls);
                Assert.IsTrue(h.Service.InitializeAsync().IsFaulted, "asked again, it fails the same way");
            }
        }

        [Test]
        public void Initialized_AThrowingHandler_DoesNotStopTheInitialization()
        {
            using (var h = new ServiceHarness())
            {
                h.Config.refreshEntitlementsOnInit = true;
                h.Start(initialize: false);
                var later = 0;
                h.Service.Initialized += () => throw new InvalidOperationException("game code failed");
                h.Service.Initialized += () => later++;

                Sync.Run(h.Service.InitializeAsync());

                Assert.IsTrue(h.Service.IsInitialized);
                Assert.AreEqual(1, h.Api.EntitlementsCalls, "the first refresh still runs");
                Sync.Run(h.Service.InitializeAsync());
                Assert.AreEqual(1, h.Store.ConnectCalls, "initialized: not connected again");
                Assert.AreEqual(0, later, "like every event: the handlers after the one that threw are not called");
            }
        }

        // ---- Providers that do not answer ----

        private sealed class HungProviders : IAttributionProvider, IAnalyticsIdProvider
        {
            public int Calls;

            public Task<IDictionary<string, string>> GetAttributionAsync()
            {
                Calls++;
                return new TaskCompletionSource<IDictionary<string, string>>().Task;
            }

            public Task<string> GetAppInstanceIdAsync()
            {
                Calls++;
                return new TaskCompletionSource<string>().Task;
            }
        }

        [Test]
        public void HungAttributionAndAnalyticsProviders_DoNotBlockVerification()
        {
            using (var h = WithGates(out var gates))
            {
                var hung = new HungProviders();
                h.Attribution = hung;
                h.Analytics = hung;
                h.Config.logLevel = BillingLogLevel.Warning;
                h.Start();

                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));
                Assert.IsEmpty(h.Api.VerifyRequests, "waiting for the app instance id");
                gates.Elapse();
                Assert.IsEmpty(h.Api.VerifyRequests, "waiting for the attribution ids");
                gates.Elapse();

                var request = h.Api.VerifyRequests.Single();
                Assert.IsNull(request.AppInstanceId);
                Assert.IsNull(request.Attribution);
                Assert.AreEqual(1, h.Store.Confirmed.Count);
                CollectionAssert.AreEqual(new[] { BillingContext.ProviderTimeout, BillingContext.ProviderTimeout }, h.Delays);
                Assert.AreEqual(2, h.Warnings.Count(w => w.Contains("did not answer")));

                // The next order waits again, but the warnings are not repeated.
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-2", "GPA.2"));
                gates.Elapse();
                gates.Elapse();
                Assert.AreEqual(2, h.Api.VerifyRequests.Count);
                Assert.AreEqual(2, h.Warnings.Count(w => w.Contains("did not answer")));
                Assert.AreEqual(4, hung.Calls);
            }
        }

        [Test]
        public void HungPlayerTokenProvider_SendsTheRequestWithoutAToken()
        {
            var gates = new Gates();
            var warnings = new List<string>();
            var cache = new PlayerTokenCache(
                new FuncPlayerTokenProvider((user, ct) => new TaskCompletionSource<string>().Task),
                new BillingLog(BillingLogLevel.Warning, (level, message) => warnings.Add(message)), gates.Delay);
            var transport = new FakeTransport { Next = new HttpResponse(200, "{\"status\":\"success\"}") };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);
            api.SetPlayerTokenProvider(cache);

            var verify = api.VerifyAsync(BillingStores.Google, new VerifyRequest { AppId = "com.test.game", UserId = "u1", ProductId = "gems_100" });
            Assert.IsFalse(verify.IsCompleted);
            Assert.IsEmpty(transport.Requests);

            gates.Elapse();

            Assert.AreEqual(VerifyStatus.Success, Sync.Run(verify).Value.Status);
            Assert.IsNull(transport.Authorization(0));
            Assert.AreEqual(1, warnings.Count);

            var again = cache.GetTokenAsync("u1", default);
            gates.Elapse();
            Assert.IsNull(Sync.Run(again));
            Assert.AreEqual(1, warnings.Count, "reported once");
        }

        [Test]
        public void AppleAds_ASlowAnswer_DoesNotHoldTheOtherAttributionIds()
        {
            var answer = new TaskCompletionSource<(long Status, string Body)>();
            var timeouts = new Queue<TaskCompletionSource<bool>>();
            var provider = new AppleAdsAttributionProvider(
                new FuncAttributionProvider(() => new Dictionary<string, string> { [AttributionKeys.AppsFlyerId] = "af-1" }),
                new MemoryBillingStorage(), () => "token", _ => answer.Task,
                delay =>
                {
                    var gate = new TaskCompletionSource<bool>();
                    timeouts.Enqueue(gate);
                    return gate.Task;
                },
                new BillingLog(BillingLogLevel.None));

            var first = provider.GetAttributionAsync();
            Assert.IsFalse(first.IsCompleted);
            timeouts.Dequeue().SetResult(true);

            var values = Sync.Run(first);
            Assert.AreEqual("af-1", values[AttributionKeys.AppsFlyerId]);
            Assert.IsFalse(values.ContainsKey(AttributionKeys.AppleAdsCampaignId));

            // Apple answers later: the next request has the campaign.
            answer.SetResult((200, "{\"attribution\":true,\"campaignId\":542370539}"));
            Assert.AreEqual("542370539", Sync.Run(provider.GetAttributionAsync())[AttributionKeys.AppleAdsCampaignId]);
        }

        // ---- Orders the server could not be asked about ----

        [Test]
        public void GivenUpOrder_IsVerifiedAgain_WhenTheAppReturnsToTheForeground()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));
                Assert.AreEqual(h.Config.retryDelaysSeconds.Length + 1, h.Api.VerifyRequests.Count);
                Assert.IsEmpty(h.Store.Confirmed);

                h.Service.OnAppResumed();
                Assert.AreEqual(2 * (h.Config.retryDelaysSeconds.Length + 1), h.Api.VerifyRequests.Count, "still offline: given up again");

                h.Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success);
                h.Service.OnAppResumed();
                Assert.AreEqual(1, h.Store.Confirmed.Count);
                Assert.AreEqual(1, h.Succeeded.Count);

                var requests = h.Api.VerifyRequests.Count;
                h.Service.OnAppResumed();
                Assert.AreEqual(requests, h.Api.VerifyRequests.Count, "verified: nothing is left to retry");
            }
        }

        [Test]
        public void GivenUpOrder_IsVerifiedAgain_WhenTheStoreReconnects()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));
                Assert.IsEmpty(h.Store.Confirmed);

                h.Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success);
                h.Store.Disconnect();

                Assert.IsTrue(h.Service.IsInitialized, "reconnected");
                Assert.AreEqual(1, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void GivenUpOrder_KeepsItsPaywall_AndLaterOrdersOfTheProductDoNotGetIt()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);
                var result = Sync.Run(h.Service.PurchaseAsync("gems_100", new PurchaseOptions { Paywall = Paywall(), OfferId = "intro" }));
                Assert.AreEqual(PurchaseStatus.PendingVerification, result.Status);
                Assert.AreEqual("main_paywall", h.Api.VerifyRequests.Last().Placement);

                // Another purchase of the product, bought without the paywall.
                h.Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success);
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-2", "GPA.2"));
                Assert.IsNull(h.Api.VerifyRequests.Last().Placement, "the paywall of the given-up order is not inherited");
                Assert.IsNull(h.Api.VerifyRequests.Last().OfferId);

                h.Service.OnAppResumed();
                var retried = h.Api.VerifyRequests.Last();
                Assert.AreEqual("token-1", retried.PurchaseToken);
                Assert.AreEqual(("main_paywall", "variant_b", "intro"), (retried.Placement, retried.VariantId, retried.OfferId),
                    "the A/B test still counts the purchase");
                Assert.AreEqual(2, h.Store.Confirmed.Count);
            }
        }

        // ---- Restore ----

        [Test]
        public void Restore_WaitsForTheVerificationsInFlight()
        {
            using (var h = WithGates(out var gates))
            {
                h.Start();
                var order = FakeStore.GoogleOrder("no_ads");
                h.Store.Owned.Add(order);
                var hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
                h.Api.Hold = hold;
                h.Store.Deliver(order);
                h.Answer(null, null, "no_ads");

                var restore = h.Service.RestoreAsync();

                Assert.IsFalse(restore.IsCompleted, "the order is being verified: what it gives is not known yet");
                Assert.AreEqual(0, h.Api.EntitlementsCalls);
                CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(h.Config.requestTimeoutSeconds) }, h.Delays, "bounded");

                hold.SetResult(FakeApi.Verify(200, VerifyStatus.Success));

                Assert.IsTrue(Sync.Run(restore).Owns("no_ads"));
                Assert.IsEmpty(h.Api.RestoreRequests, "verified by itself: not posted to /restore");
                Assert.AreEqual(1, h.Store.Confirmed.Count);
            }
        }

        [Test]
        public void Restore_DoesNotWaitForeverForAVerification()
        {
            using (var h = WithGates(out var gates))
            {
                h.Start();
                var order = FakeStore.GoogleOrder("no_ads");
                h.Store.Owned.Add(order);
                h.Api.Hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
                h.Store.Deliver(order);

                var restore = h.Service.RestoreAsync();
                Assert.IsFalse(restore.IsCompleted);
                gates.Elapse();

                Assert.IsTrue(restore.IsCompleted, "answered without the order that is still being verified");
                Assert.AreEqual(1, h.Api.EntitlementsCalls);
            }
        }

        // ---- The foreground refresh ----

        [Test]
        public void Resume_AfterAFailedRefresh_RefreshesAgain()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Service.OnAppResumed();
                Assert.AreEqual(1, h.Api.EntitlementsCalls);
                h.Service.OnAppResumed();
                Assert.AreEqual(2, h.Api.EntitlementsCalls, "the server did not answer: no reason to wait five minutes");

                h.Answer(null, new Dictionary<string, double> { ["premium"] = 10 });
                h.Service.OnAppResumed();
                Assert.AreEqual(3, h.Api.EntitlementsCalls);
                h.Service.OnAppResumed();
                Assert.AreEqual(3, h.Api.EntitlementsCalls, "answered: throttled");
            }
        }

        // ---- Whose result is it ----

        [Test]
        public void AnOlderOrderOfTheProduct_DoesNotCompleteANewPurchase()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
                h.Api.Hold = hold;
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-old", "GPA.OLD"));

                // The player buys the product again while the old order is still being verified.
                var purchase = h.Service.PurchaseAsync("gems_100", Paywall());
                Assert.IsFalse(purchase.IsCompleted);

                hold.SetResult(FakeApi.Verify(200, VerifyStatus.Success));
                Assert.AreEqual(1, h.Succeeded.Count, "the old order is reported by the event");
                Assert.IsFalse(purchase.IsCompleted, "but it is not the result of the new purchase");

                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-new", "GPA.NEW"));
                Assert.AreEqual(PurchaseStatus.Success, Sync.Run(purchase).Status);
                var requests = h.Api.VerifyRequests;
                Assert.IsNull(requests[0].Placement);
                Assert.AreEqual("tok-new", requests[1].PurchaseToken);
                Assert.AreEqual("main_paywall", requests[1].Placement, "the paywall goes with the purchase that had it");
            }
        }

        [Test]
        public void AnOlderOrderDeliveredAgain_DoesNotTakeThePaywallOfANewPurchase()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
                h.Api.Hold = hold;
                var old = FakeStore.GoogleOrder("gems_100", "tok-old", "GPA.OLD");
                h.Store.Deliver(old);
                var purchase = h.Service.PurchaseAsync("gems_100", Paywall());

                // The old order is not paid yet; the store delivers it again once it is.
                hold.SetResult(FakeApi.Verify(200, VerifyStatus.PendingOrCanceled));
                Assert.IsFalse(purchase.IsCompleted);
                h.Store.Deliver(old);

                Assert.AreEqual(2, h.Api.VerifyRequests.Count);
                Assert.IsNull(h.Api.VerifyRequests.Last().Placement, "it was bought before the paywall was shown");
                Assert.IsFalse(purchase.IsCompleted);

                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-new", "GPA.NEW"));
                Assert.AreEqual("main_paywall", h.Api.VerifyRequests.Last().Placement);
                Assert.AreEqual(PurchaseStatus.Success, Sync.Run(purchase).Status);
            }
        }

        [Test]
        public void AGivenUpOrderOfTheProduct_VerifiedLater_DoesNotCompleteANewPurchase()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.DefaultVerify = FakeApi.TransportError();
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-old", "GPA.OLD"));
                h.Api.DefaultVerify = FakeApi.Verify(200, VerifyStatus.Success);

                var purchase = h.Service.PurchaseAsync("gems_100", Paywall());
                h.Service.OnAppResumed();

                Assert.AreEqual(1, h.Store.Confirmed.Count, "the old order was verified");
                Assert.IsNull(h.Api.VerifyRequests.Last().Placement, "without the paywall of the new purchase");
                Assert.IsFalse(purchase.IsCompleted);

                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-new", "GPA.NEW"));
                Assert.AreEqual(PurchaseStatus.Success, Sync.Run(purchase).Status);
                Assert.AreEqual("main_paywall", h.Api.VerifyRequests.Last().Placement);
            }
        }

        // ---- Dispose ----

        [Test]
        public void AfterDispose_NothingCallsTheStoreOrTheServer()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Store.Owned.Add(FakeStore.GoogleOrder("no_ads"));
                h.Service.Dispose();

                Assert.IsFalse(h.Service.IsInitialized);
                Assert.AreEqual(PurchaseStatus.NotInitialized, Sync.Run(h.Service.PurchaseAsync("gems_100")).Status);
                Assert.Throws<ObjectDisposedException>(() => Sync.Run(h.Service.RestoreAsync()));
                Assert.Throws<ObjectDisposedException>(() => Sync.Run(h.Service.RefreshCustomerInfoAsync()));
                Assert.IsEmpty(h.Store.PurchaseCalls);
                Assert.IsEmpty(h.Api.RestoreRequests);
                Assert.AreEqual(0, h.Api.EntitlementsCalls);
            }
        }

        // ---- The store connection ----

        [Test]
        public void ADisconnect_WhileAReconnectionIsFinishing_IsNotLost()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var disconnects = new List<string>();
                var reconnects = 0;
                h.Service.StoreDisconnected += disconnects.Add;
                h.Service.StoreReconnected += () =>
                {
                    // The store drops again while the game is being told that it is back.
                    if (++reconnects == 1)
                        h.Store.Disconnect("dropped again");
                };

                h.Store.Disconnect("first");

                CollectionAssert.AreEqual(new[] { "first", "dropped again" }, disconnects);
                Assert.AreEqual(2, reconnects, "it reconnected once more");
                Assert.IsTrue(h.Service.IsInitialized);
                Assert.AreEqual(3, h.Store.ConnectCalls);
            }
        }

        // ---- Prices of orders that arrive during initialization ----

        [Test]
        public void AnOrderDeliveredWhileTheProductsLoad_IsVerifiedWithItsPrice()
        {
            using (var h = WithGates(out var gates))
            {
                h.Store.HoldProducts = new TaskCompletionSource<bool>();
                h.Start(initialize: false);
                var init = h.Service.InitializeAsync();
                Assert.IsFalse(init.IsCompleted);

                // Stores deliver unfinished purchases right after they connect.
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));
                Assert.IsEmpty(h.Api.VerifyRequests, "the price is not known yet");
                CollectionAssert.AreEqual(new[] { OrderProcessor.ProductsWait }, h.Delays, "bounded");

                h.Store.HoldProducts.SetResult(true);

                Sync.Run(init);
                var request = h.Api.VerifyRequests.Single();
                Assert.AreEqual(990_000, request.PriceMicros);
                Assert.AreEqual("USD", request.Currency);
            }
        }

        [Test]
        public void AnOrderDeliveredWhileTheProductsLoad_DoesNotWaitForeverForThem()
        {
            using (var h = WithGates(out var gates))
            {
                h.Store.HoldProducts = new TaskCompletionSource<bool>();
                h.Start(initialize: false);
                var init = h.Service.InitializeAsync();
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));

                gates.Elapse();

                Assert.AreEqual(0, h.Api.VerifyRequests.Single().PriceMicros, "verified without the price");
                Assert.AreEqual(1, h.Store.Confirmed.Count);
                h.Store.HoldProducts.SetResult(true);
                Sync.Run(init);
            }
        }

        // ---- Callbacks that must run on the main thread ----

        private sealed class QueueContext : SynchronizationContext
        {
            public readonly Queue<Action> Posted = new Queue<Action>();

            public override void Post(SendOrPostCallback d, object state) => Posted.Enqueue(() => d(state));
        }

        [Test]
        public void MainThreadPost_RunsOnTheOwningThread()
        {
            var context = new QueueContext();
            var ran = 0;

            MainThreadPost.Run(null, () => ran++);
            Assert.AreEqual(1, ran, "nothing to post to: run in place");

            // Another thread (a token cancelled from a timer or the thread pool).
            var other = new Thread(() => MainThreadPost.Run(context, () => ran++));
            other.Start();
            other.Join();
            Assert.AreEqual(1, ran, "not run on the other thread");
            Assert.AreEqual(1, context.Posted.Count);
            context.Posted.Dequeue()();
            Assert.AreEqual(2, ran);

            var previous = SynchronizationContext.Current;
            SynchronizationContext.SetSynchronizationContext(context);
            try
            {
                MainThreadPost.Run(context, () => ran++);
                Assert.AreEqual(3, ran, "already on the owning thread: run in place");
                Assert.IsEmpty(context.Posted);
            }
            finally
            {
                SynchronizationContext.SetSynchronizationContext(previous);
            }
        }
    }
}
