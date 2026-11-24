using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Foreground refresh, user id changes, store reconnection, purchase options.</summary>
    public sealed class ServiceLifecycleTests
    {
        private static Paywall Paywall(string placement = "main_paywall") =>
            new Paywall(placement, "pw_1", "Main", "variant_b", "test_7", null, null, "{}", false,
                new DateTime(2027, 1, 1, 0, 0, 0, DateTimeKind.Utc));

        [Test]
        public void Resume_RefreshesTheCustomerInfo_AtMostEveryFiveMinutes()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Answer(null, new Dictionary<string, double> { ["premium"] = 10 });
                Assert.AreEqual(0, h.Api.EntitlementsCalls);

                h.Service.OnAppResumed();
                Assert.AreEqual(1, h.Api.EntitlementsCalls);
                Assert.IsTrue(h.Service.HasAccess("premium"));

                h.Now = h.Now.AddMinutes(4);
                h.Service.OnAppResumed();
                Assert.AreEqual(1, h.Api.EntitlementsCalls, "within five minutes of the last refresh");

                h.Now = h.Now.AddMinutes(2);
                h.Service.OnAppResumed();
                Assert.AreEqual(2, h.Api.EntitlementsCalls);

                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                h.Now = h.Now.AddMinutes(1);
                h.Service.OnAppResumed();
                Assert.AreEqual(3, h.Api.EntitlementsCalls, "any refresh counts");
            }
        }

        [Test]
        public void Resume_WithoutAPlayer_DoesNothing()
        {
            using (var h = new ServiceHarness { Users = new StaticUserIdProvider() })
            {
                h.Start();
                h.Service.OnAppResumed();
                Assert.AreEqual(0, h.Api.EntitlementsCalls);
            }
        }

        [Test]
        public void FuncUserIdProvider_ChangesAreNoticed_LikeSetUserId()
        {
            string id = null;
            using (var h = new ServiceHarness { Users = new FuncUserIdProvider(() => id) })
            {
                var cached = FakeApi.Info(null, new Dictionary<string, long> { ["no_ads"] = 0 }, "no_ads");
                CustomerInfoCache.Save(h.Storage, ServiceHarness.CacheScope, "player-2", cached, h.Now, null);
                h.Start();
                h.Store.Deliver(FakeStore.GoogleOrder("no_ads"));
                Assert.IsEmpty(h.Api.VerifyRequests, "the order waits for a player");

                // Login: the provider now returns an id; the once-a-second tick notices it.
                id = "player-2";
                h.Service.OnTick();
                Assert.AreEqual("player-2", h.Api.VerifyRequests.Single().UserId, "the waiting order is verified");
                Assert.AreEqual("player-2", h.Store.BoundUserId);
                Assert.AreEqual("player-2", h.Service.CustomerInfo.UserId, "the new player's cache is loaded");
                Assert.IsTrue(h.Service.HasAccess("no_ads"));

                // Account switch noticed before a server request.
                id = "player-3";
                h.Store.OnPurchase = s => FakeStore.GoogleOrder(s, "tok-3", "GPA.3");
                Sync.Run(h.Service.PurchaseAsync("gems_100"));
                Assert.AreEqual("player-3", h.Api.VerifyRequests.Last().UserId);
                Assert.AreEqual("player-3", h.Store.BoundUserId);
                Assert.IsFalse(h.Service.HasAccess("no_ads"), "player-2's access is gone");

                // ... and on resume, which refreshes the new player's customer info.
                var calls = h.Api.EntitlementsCalls;
                id = "player-4";
                h.Service.OnAppResumed();
                Assert.AreEqual(calls + 1, h.Api.EntitlementsCalls);
                Assert.AreEqual("player-4", h.Store.BoundUserId);
            }
        }

        [Test]
        public void CancellingTheWait_KeepsThePaywallOfTheStorePurchase()
        {
            using (Sync.InlineContinuations())
            using (var h = new ServiceHarness())
            {
                h.Start();
                using (var cts = new CancellationTokenSource())
                {
                    var purchase = h.Service.PurchaseAsync("gems_100", Paywall(), cts.Token);
                    cts.Cancel();
                    Assert.IsTrue(purchase.IsCanceled);
                }

                // The player finishes paying in the store's dialog after the game stopped waiting.
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));

                var request = h.Api.VerifyRequests.Single();
                Assert.AreEqual(("main_paywall", "variant_b", "test_7", "pw_1"),
                    (request.Placement, request.VariantId, request.TestId, request.PaywallId), "the A/B test still counts it");
                Assert.AreEqual(1, h.Succeeded.Count);

                // The purchase ended: a later order of the product is not attributed to the paywall.
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100", "tok-2", "GPA.2"));
                Assert.IsNull(h.Api.VerifyRequests.Last().Placement);
            }
        }

        [Test]
        public void FailedPurchase_DropsThePaywall()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Store.OnPurchase = id =>
                {
                    h.Store.Fail(id, cancelled: true);
                    return null;
                };
                Assert.AreEqual(PurchaseStatus.Cancelled, Sync.Run(h.Service.PurchaseAsync("gems_100", Paywall())).Status);
                h.Store.Deliver(FakeStore.GoogleOrder("gems_100"));
                Assert.IsNull(h.Api.VerifyRequests.Single().Placement);
            }
        }

        [Test]
        public void AppleRestore_DoesNotVerifyAnOrderTwice()
        {
            using (var h = new ServiceHarness())
            {
                h.Store.Platform = BillingPlatform.Apple;
                h.Start();
                var order = FakeStore.AppleOrder("no_ads", "2000000009");
                h.Store.Owned.Add(order);
                h.Store.RestoreRedelivers = true;
                var hold = new TaskCompletionSource<ApiResult<VerifyResponse>>();
                h.Api.Hold = hold;
                h.Answer(null, null, "no_ads");

                var restore = h.Service.RestoreAsync();

                Assert.IsTrue(h.Store.RestoreCalled);
                Assert.AreEqual(1, h.Api.VerifyRequests.Count, "RestoreTransactions re-delivered it: being verified");
                Assert.IsEmpty(h.Api.RestoreRequests, "not posted to /restore while it is verified");
                Assert.IsTrue(Sync.Run(restore).Owns("no_ads"));

                // The same transaction under another key is not verified again either.
                h.Store.Deliver(new StoreOrder(BillingPlatform.Apple, "no_ads", "2000000009", "another-key"));
                Assert.AreEqual(1, h.Api.VerifyRequests.Count);

                hold.SetResult(FakeApi.Verify(200, VerifyStatus.Success));
                Assert.AreEqual(1, h.Store.Confirmed.Count);

                // Once finished it may be verified again (e.g. a later restore).
                h.Store.Deliver(order);
                Assert.AreEqual(2, h.Api.VerifyRequests.Count);
            }
        }

        [Test]
        public void StoreDisconnect_ReconnectsWithBackoff_AndRefreshes()
        {
            using (var h = new ServiceHarness())
            {
                var gates = new Queue<TaskCompletionSource<bool>>();
                h.Start();
                h.Answer(null, new Dictionary<string, double> { ["premium"] = 10 });
                h.Delay = (d, ct) =>
                {
                    var gate = new TaskCompletionSource<bool>();
                    gates.Enqueue(gate);
                    return gate.Task;
                };
                var disconnects = new List<string>();
                var reconnects = 0;
                h.Service.StoreDisconnected += disconnects.Add;
                h.Service.StoreReconnected += () => reconnects++;

                h.Store.Disconnect("billing unavailable");

                CollectionAssert.AreEqual(new[] { "billing unavailable" }, disconnects);
                Assert.IsFalse(h.Service.IsInitialized);
                Assert.AreEqual(PurchaseStatus.NotInitialized, Sync.Run(h.Service.PurchaseAsync("gems_100")).Status);
                h.Store.Disconnect("again");
                Assert.AreEqual(1, disconnects.Count, "already reconnecting");

                var connects = h.Store.ConnectCalls;
                h.Store.FailConnects = 1;
                gates.Dequeue().SetResult(true);
                Assert.AreEqual(connects + 1, h.Store.ConnectCalls);
                Assert.IsFalse(h.Service.IsInitialized, "the first attempt failed");

                gates.Dequeue().SetResult(true);
                Assert.IsTrue(h.Service.IsInitialized);
                Assert.AreEqual(1, reconnects);
                CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(5), TimeSpan.FromSeconds(10) }, h.Delays);
                Assert.AreEqual(1, h.Api.EntitlementsCalls, "the customer info is refreshed after reconnecting");
                Assert.IsTrue(h.Service.HasAccess("premium"));

                h.Store.Disconnect("later");
                Assert.AreEqual(2, disconnects.Count, "a new disconnect after reconnecting");
            }
        }

        [Test]
        public void StoreDisconnect_BeforeInitialization_IsIgnored()
        {
            using (var h = new ServiceHarness())
            {
                h.Start(initialize: false);
                var disconnects = 0;
                h.Service.StoreDisconnected += _ => disconnects++;
                h.Store.Disconnect();
                Assert.AreEqual(0, disconnects);
                Assert.IsEmpty(h.Delays);
            }
        }

        [Test]
        public void ReconnectDelay_DoublesUpToFiveMinutes()
        {
            Assert.AreEqual(TimeSpan.FromSeconds(5), BillingService.ReconnectDelay(0));
            Assert.AreEqual(TimeSpan.FromSeconds(40), BillingService.ReconnectDelay(3));
            Assert.AreEqual(TimeSpan.FromSeconds(300), BillingService.ReconnectDelay(10));
        }

        [Test]
        public void PurchaseOptions_SendTheOfferId()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);
                var result = Sync.Run(h.Service.PurchaseAsync("premium", new PurchaseOptions { OfferId = "intro-7d", Paywall = Paywall() }));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                var request = h.Api.VerifyRequests.Single();
                Assert.AreEqual("intro-7d", request.OfferId);
                Assert.AreEqual("main_paywall", request.Placement);
                CollectionAssert.AreEqual(new[] { "premium_android" }, h.Store.PurchaseCalls);
            }
        }

        [Test]
        public void PurchaseOptions_ReplaceASubscription()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id, "tok-year", "GPA.9");
                var result = Sync.Run(h.Service.PurchaseAsync("premium_year",
                    new PurchaseOptions { ReplacesProductId = "premium", ReplacementMode = ReplacementMode.ChargeFullPrice }));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                Assert.AreEqual(("premium_year", "premium_android", ReplacementMode.ChargeFullPrice), h.Store.ReplaceCalls.Single());
                Assert.IsEmpty(h.Store.PurchaseCalls);
                Assert.AreEqual("premium_year", h.Api.VerifyRequests.Single().ProductId);
            }
        }

        [Test]
        public void PurchaseOptions_InvalidReplacement_Fails()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                PurchaseResult Buy(string id, string replaces) =>
                    Sync.Run(h.Service.PurchaseAsync(id, new PurchaseOptions { ReplacesProductId = replaces }));

                StringAssert.Contains("unknown product", Buy("premium_year", "nope").Message);
                StringAssert.Contains("only a subscription", Buy("gems_100", "premium").Message);
                StringAssert.Contains("only a subscription", Buy("premium_year", "no_ads").Message);
                StringAssert.Contains("itself", Buy("premium", "premium_android").Message);
                Assert.IsEmpty(h.Store.ReplaceCalls);
                Assert.IsEmpty(h.Store.PurchaseCalls);
            }
        }

        [Test]
        public void DeferredSubscriptionChange_IsPendingUntilTheRenewal()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Store.OnPurchase = id =>
                {
                    h.Store.DeferChange(id);
                    return null;
                };
                var result = Sync.Run(h.Service.PurchaseAsync("premium_year",
                    new PurchaseOptions { ReplacesProductId = "premium", ReplacementMode = ReplacementMode.Deferred }));

                Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
                StringAssert.Contains("next renewal", result.Message);
                Assert.AreEqual(1, h.Pending.Count);
                Assert.AreEqual(1, h.Api.EntitlementsCalls, "the customer info shows renews_to");
                Assert.IsEmpty(h.Api.VerifyRequests);
            }
        }

        [Test]
        public void StoreOffers_ReachTheProducts()
        {
            using (var h = new ServiceHarness())
            {
                h.Store.Offers = new[]
                {
                    new StoreOffer("trial-7d", "tok", new[]
                    {
                        new PricingPhase(0m, "", "USD", "P1W", 1),
                        new PricingPhase(4.99m, "$4.99", "USD", "P1M", 0),
                    }),
                };
                h.Store.IntroEligible = true;
                h.Config.FindById("premium").WithFallbackPeriod("P1M");
                h.Start();

                var premium = h.Service.GetProduct("premium");
                Assert.IsTrue(premium.IsFallbackPeriod, "the fallback path keeps the offers");
                Assert.AreEqual(true, premium.IntroEligible);
                var offer = premium.Offers.Single();
                Assert.AreEqual(("trial-7d", "tok", true), (offer.Id, offer.Token, offer.HasFreeTrial));
                Assert.AreEqual(("P1W", 1), (offer.PricingPhases[0].Period, offer.PricingPhases[0].Cycles));
                Assert.IsEmpty(h.Service.GetProduct("gems_100").Offers);
                Assert.IsNull(h.Service.GetProduct("gems_100").IntroEligible);
            }
        }
    }

    public sealed class StoreOfferParserTests
    {
        [Test]
        public void Google_TrialIntroAndBasePhases()
        {
            var (offers, eligible) = StoreOfferParser.Google("P1M", 4.99m, "$4.99", "USD", "P1W", "$0.99", "P1M", 3,
                "{\"productId\":\"premium\",\"introductoryPriceAmountMicros\":990000}");
            var phases = offers.Single().PricingPhases;
            Assert.AreEqual(3, phases.Count);
            Assert.AreEqual((0m, "P1W", 1, true), (phases[0].Price, phases[0].Period, phases[0].Cycles, phases[0].IsFreeTrial));
            Assert.AreEqual((0.99m, "$0.99", "P1M", 3), (phases[1].Price, phases[1].LocalizedPrice, phases[1].Period, phases[1].Cycles));
            Assert.AreEqual((4.99m, "P1M", 0), (phases[2].Price, phases[2].Period, phases[2].Cycles));
            Assert.IsNull(offers.Single().Id, "Unity IAP 5 does not expose the offer id");
            Assert.IsNull(offers.Single().Token, "nor the offer token");
            Assert.AreEqual(true, eligible);
        }

        [Test]
        public void Google_BasePlanOnly_AndNotASubscription()
        {
            var (offers, eligible) = StoreOfferParser.Google("P1Y", 29.99m, "$29.99", "USD", "", "", "", 0, "{}");
            Assert.AreEqual(1, offers.Single().PricingPhases.Count);
            Assert.IsFalse(offers.Single().HasFreeTrial);
            Assert.IsNull(eligible, "unknown when there is no trial or intro phase");

            var (none, unknown) = StoreOfferParser.Google(null, 0.99m, "$0.99", "USD", null, null, null, 0, null);
            Assert.IsEmpty(none);
            Assert.IsNull(unknown);
        }

        [Test]
        public void Apple_PeriodIntroOfferAndEligibility()
        {
            const string details = "{\"subscriptionNumberOfUnits\":\"1\",\"subscriptionPeriodUnit\":\"2\",\"isEligibleForIntroOffer\":\"true\"," +
                                   "\"introductoryPrice\":\"0\",\"introductoryType\":\"FreeTrial\",\"introductoryOfferId\":null," +
                                   "\"introductoryPriceNumberOfPeriods\":\"1\",\"numberOfUnits\":\"1\",\"unit\":\"1\"}";
            var (period, offers, eligible) = StoreOfferParser.Apple(details, 4.99m, "$4.99", "USD");
            Assert.AreEqual("P1M", period);
            Assert.AreEqual(true, eligible);
            var phases = offers.Single().PricingPhases;
            Assert.AreEqual((0m, "P1W", 1, true), (phases[0].Price, phases[0].Period, phases[0].Cycles, phases[0].IsFreeTrial));
            Assert.AreEqual((4.99m, "P1M", 0), (phases[1].Price, phases[1].Period, phases[1].Cycles));
        }

        [Test]
        public void Apple_PayAsYouGoAndPayUpFront()
        {
            const string payAsYouGo = "{\"subscriptionNumberOfUnits\":1,\"subscriptionPeriodUnit\":2,\"isEligibleForIntroOffer\":false," +
                                      "\"introductoryPrice\":\"0.99\",\"introductoryType\":\"PayAsYouGo\",\"introductoryOfferId\":\"intro\"," +
                                      "\"introductoryPriceNumberOfPeriods\":3,\"numberOfUnits\":1,\"unit\":2}";
            var (_, offers, eligible) = StoreOfferParser.Apple(payAsYouGo, 4.99m, "$4.99", "USD");
            Assert.AreEqual(false, eligible);
            Assert.AreEqual("intro", offers.Single().Id);
            var intro = offers.Single().PricingPhases[0];
            Assert.AreEqual((0.99m, "P1M", 3), (intro.Price, intro.Period, intro.Cycles));

            var upFront = payAsYouGo.Replace("PayAsYouGo", "PayUpFront").Replace("\"introductoryPrice\":\"0.99\"", "\"introductoryPrice\":\"9.99\"");
            var front = StoreOfferParser.Apple(upFront, 4.99m, "$4.99", "USD").Offers.Single().PricingPhases[0];
            Assert.AreEqual((9.99m, "P3M", 1), (front.Price, front.Period, front.Cycles));
        }

        [Test]
        public void Apple_NotASubscriptionOrNoIntroOffer()
        {
            Assert.AreEqual("", StoreOfferParser.Apple("{\"localizedPrice\":\"0.99\"}", 0.99m, "$0.99", "USD").Period);
            Assert.AreEqual("", StoreOfferParser.Apple("not json", 0.99m, "$0.99", "USD").Period);
            Assert.AreEqual("", StoreOfferParser.Apple(null, 0.99m, "$0.99", "USD").Period);
            var (period, offers, eligible) = StoreOfferParser.Apple(
                "{\"subscriptionNumberOfUnits\":\"1\",\"subscriptionPeriodUnit\":\"3\",\"introductoryPrice\":\"\",\"numberOfUnits\":\"\",\"unit\":\"\"}",
                29.99m, "$29.99", "USD");
            Assert.AreEqual("P1Y", period);
            Assert.IsEmpty(offers);
            Assert.IsNull(eligible, "StoreKit 1 does not say");
        }

        [Test]
        public void StoreKitPeriods()
        {
            Assert.AreEqual("P3D", StoreOfferParser.Period("3", "0"));
            Assert.AreEqual("P1W", StoreOfferParser.Period("1", "1"));
            Assert.AreEqual("P6M", StoreOfferParser.Period("6", "2"));
            Assert.AreEqual("P1Y", StoreOfferParser.Period("1", "3"));
            Assert.AreEqual("", StoreOfferParser.Period("0", "2"));
            Assert.AreEqual("", StoreOfferParser.Period("1", "9"));
        }
    }
}
