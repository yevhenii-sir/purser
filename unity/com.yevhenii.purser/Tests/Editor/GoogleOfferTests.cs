using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Google Play subscription offers chosen by the game: <see cref="GooglePlayOfferClient"/> on a fake of the
    /// Android plugin, and the service around it (<see cref="IBillingService.GetGoogleOffersAsync"/>,
    /// <see cref="PurchaseOptions.GoogleOfferToken"/>).</summary>
    public sealed class GoogleOfferTests
    {
        private const string User = "player-42";

        private BillingConfig m_Config;
        private FakeStore m_Store;
        private FakeApi m_Api;
        private BillingService m_Service;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("premium", BillingProductType.Subscription)
                    .WithStoreId(BillingStores.Google, "premium_android"),
                new BillingProductConfig("premium_yearly", BillingProductType.Subscription));
            m_Config.WithBuild(BillingStores.Google, "com.test.game");
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Store = new FakeStore { Platform = BillingPlatform.Google };
            m_Api = new FakeApi();
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private BillingService CreateService()
        {
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = m_Store,
                Api = m_Api,
                UserIdProvider = new StaticUserIdProvider(User),
                Storage = new MemoryBillingStorage(),
                Delay = (delay, ct) => Task.CompletedTask,
                IsEditor = false,
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        private static (GooglePlayOfferClient Client, FakeGooglePlayBillingBridge Bridge, List<object> Events) Client()
        {
            var bridge = new FakeGooglePlayBillingBridge();
            var client = new GooglePlayOfferClient(bridge, new BillingLog(BillingLogLevel.None));
            var events = new List<object>();
            client.OrderPending += order => events.Add(("pending", order));
            client.OrderDeferred += order => events.Add(("deferred", order));
            client.PurchaseFailed += failure => events.Add(failure);
            client.ChangeDeferred += storeId => events.Add(("change deferred", storeId));
            return (client, bridge, events);
        }

        private static GooglePlayBillingFlow Flow(string token = FakeGooglePlayBillingBridge.IntroToken) =>
            new GooglePlayBillingFlow { ProductId = "premium_android", OfferToken = token };

        // ---- GooglePlayOfferClient ----

        [Test]
        public void Offers_ListBasePlansAndOffersWithTheirPhases()
        {
            var (client, bridge, _) = Client();

            var offers = Sync.Run(client.GetOffersAsync("premium_android", CancellationToken.None));

            Assert.AreEqual("premium_android", bridge.OfferQueries.Single());
            Assert.AreEqual(2, offers.Count);
            var basePlan = offers[0];
            Assert.IsNull(basePlan.Id, "a base plan has no offer id");
            Assert.AreEqual(("monthly", "monthly", FakeGooglePlayBillingBridge.MonthlyToken),
                (basePlan.BasePlanId, basePlan.RevenueId, basePlan.Token));
            Assert.AreEqual(0, basePlan.PricingPhases.Single().Cycles, "the regular price recurs");

            var intro = offers[1];
            Assert.AreEqual(("intro", "monthly", "intro"), (intro.Id, intro.BasePlanId, intro.RevenueId));
            CollectionAssert.AreEqual(new[] { "new-players", "intro" }, intro.Tags);
            Assert.IsTrue(intro.HasFreeTrial);
            var phases = intro.PricingPhases;
            Assert.AreEqual(3, phases.Count);
            Assert.AreEqual(("P1W", 1, true), (phases[0].Period, phases[0].Cycles, phases[0].IsFreeTrial));
            Assert.AreEqual((0.99m, "$0.99", "USD", "P1M", 3),
                (phases[1].Price, phases[1].LocalizedPrice, phases[1].Currency, phases[1].Period, phases[1].Cycles));
            Assert.AreEqual((4.99m, 0), (phases[2].Price, phases[2].Cycles));
        }

        [Test]
        public void Offers_ErrorAnswer_Throws()
        {
            var (client, bridge, _) = Client();
            bridge.OffersJson = FakeGooglePlayBillingBridge.Error(3, "Billing service unavailable on device.");

            var e = Assert.Throws<BillingException>(() => Sync.Run(client.GetOffersAsync("premium_android", CancellationToken.None)));
            StringAssert.Contains("BILLING_UNAVAILABLE (3)", e.Message);
            StringAssert.Contains("unavailable on device", e.Message);
        }

        [Test]
        public void Purchase_Purchased_IsAnOrderWithTheTokenAndOrderId()
        {
            var (client, bridge, events) = Client();

            client.Purchase(Flow());

            Assert.AreEqual(FakeGooglePlayBillingBridge.IntroToken, bridge.Flows.Single().OfferToken);
            var (kind, order) = ((string, StoreOrder))events.Single();
            Assert.AreEqual("pending", kind);
            Assert.AreEqual((BillingPlatform.Google, "premium_android", "GPA.3333-4444", "offer-purchase-token"),
                (order.Platform, order.StoreProductId, order.TransactionId, order.PurchaseToken));
            var native = (GooglePlayPurchase)order.Native;
            Assert.AreEqual(FakeGooglePlayBillingBridge.IntroToken, native.OfferToken);
            Assert.IsFalse(native.IsAcknowledged, "the plugin never acknowledges");
        }

        [Test]
        public void Purchase_PendingPayment_IsDeferred()
        {
            var (client, bridge, events) = Client();
            bridge.OnPurchase = flow => FakeGooglePlayBillingBridge.Purchased(flow.ProductId, state: 2);

            client.Purchase(Flow());

            var (kind, order) = ((string, StoreOrder))events.Single();
            Assert.AreEqual("deferred", kind);
            Assert.IsTrue(((GooglePlayPurchase)order.Native).IsPending);
        }

        [Test]
        public void Purchase_Cancelled_FailsAsCancelled()
        {
            var (client, bridge, events) = Client();
            bridge.OnPurchase = _ => FakeGooglePlayBillingBridge.Error(1, "");

            client.Purchase(Flow());

            var failure = (StorePurchaseFailure)events.Single();
            Assert.IsTrue(failure.UserCancelled);
            Assert.AreEqual("premium_android", failure.StoreProductId);
        }

        [Test]
        public void Purchase_OfferNoLongerAvailable_FailsWithTheReason()
        {
            var (client, bridge, events) = Client();
            bridge.OnPurchase = _ => FakeGooglePlayBillingBridge.Error(4, "the offer is not available to this player");

            client.Purchase(Flow());

            var failure = (StorePurchaseFailure)events.Single();
            Assert.IsFalse(failure.UserCancelled);
            Assert.AreEqual("ITEM_UNAVAILABLE (4): the offer is not available to this player", failure.Reason);
        }

        [Test]
        public void Purchase_UnreadableAnswer_Fails()
        {
            var (client, bridge, events) = Client();
            bridge.OnPurchase = _ => "not json";

            client.Purchase(Flow());

            StringAssert.Contains("unreadable", ((StorePurchaseFailure)events.Single()).Reason);
        }

        [Test]
        public void Purchase_NoPurchaseReported_IsADeferredChangeOnlyForADeferredReplacement()
        {
            var (client, bridge, events) = Client();
            bridge.OnPurchase = _ => @"{""code"":0,""purchases"":[]}";

            client.Purchase(new GooglePlayBillingFlow
            {
                ProductId = "premium_android", OfferToken = "t", OldProductId = "basic", ReplacementMode = ReplacementMode.Deferred,
            });
            client.Purchase(Flow());

            Assert.AreEqual(("change deferred", "premium_android"), events[0]);
            StringAssert.Contains("no purchase", ((StorePurchaseFailure)events[1]).Reason);
        }

        [Test]
        public void Purchase_Unsupported_FailsWithoutCallingThePlugin()
        {
            var (client, bridge, events) = Client();
            bridge.IsAvailable = false;

            client.Purchase(Flow());

            Assert.IsEmpty(bridge.Flows);
            Assert.AreEqual(GooglePlayOfferClient.NotSupportedMessage, ((StorePurchaseFailure)events.Single()).Reason);
            Assert.Throws<BillingException>(() => Sync.Run(client.GetOffersAsync("premium_android", CancellationToken.None)));
        }

        [Test]
        public void Purchase_AnswerFromAnotherThread_IsRaisedOnTheMainThread()
        {
            var (client, bridge, events) = Client();
            bridge.OnPurchase = _ => null;
            using (var main = MainThreadQueue.Install())
            {
                client.Purchase(Flow());
                // The plugin answers on Android's main thread, not Unity's.
                using (Sync.InlineContinuations())
                    bridge.Answer(FakeGooglePlayBillingBridge.Purchased("premium_android"));
                Assert.IsEmpty(events, "posted to the main thread");

                main.RunAll();
                Assert.AreEqual(1, events.Count);
            }
        }

        // ---- The service ----

        [Test]
        public void Service_ListsTheOffersOfTheStoreProduct()
        {
            CreateService();

            Assert.IsTrue(m_Service.GoogleOffersSupported);
            var offers = Sync.Run(m_Service.GetGoogleOffersAsync("premium"));

            Assert.AreEqual("premium_android", m_Store.GoogleBilling.OfferQueries.Single());
            CollectionAssert.AreEqual(new[] { FakeGooglePlayBillingBridge.MonthlyToken, FakeGooglePlayBillingBridge.IntroToken },
                offers.Select(o => o.Token));
        }

        [Test]
        public void Service_PurchaseWithAnOfferToken_BuysThatOfferAndVerifiesIt()
        {
            CreateService();
            var intro = Sync.Run(m_Service.GetGoogleOffersAsync("premium")).Single(o => o.Id == "intro");

            var result = Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions { GoogleOfferToken = intro.Token }));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.AreEqual(("premium", "premium_android"), (result.ProductId, result.StoreProductId));
            Assert.IsEmpty(m_Store.PurchaseCalls, "not bought through Unity IAP (its first offer)");
            var flow = m_Store.GoogleBilling.Flows.Single();
            Assert.AreEqual(("premium_android", FakeGooglePlayBillingBridge.IntroToken), (flow.ProductId, flow.OfferToken));
            Assert.AreEqual(AccountIds.GoogleAccountId(User), flow.ObfuscatedAccountId, "bound to the player");
            Assert.IsNull(flow.OldProductId);

            var verify = m_Api.VerifyRequests.Single();
            Assert.AreEqual(BillingStores.Google, m_Api.VerifyStores.Single());
            Assert.AreEqual(("premium_android", "offer-purchase-token", "GPA.3333-4444"),
                (verify.ProductId, verify.PurchaseToken, verify.OrderId));
            Assert.AreEqual("intro", verify.OfferId, "the server records the offer");
            Assert.IsInstanceOf<GooglePlayPurchase>(m_Store.Confirmed.Single().Native,
                "finished like any order (UnityIapStoreAdapter does nothing for it: the server acknowledges)");
        }

        [Test]
        public void Service_PurchaseOfABasePlan_RecordsTheBasePlan()
        {
            CreateService();
            Sync.Run(m_Service.GetGoogleOffersAsync("premium"));

            Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions { GoogleOfferToken = FakeGooglePlayBillingBridge.MonthlyToken }));

            Assert.AreEqual("monthly", m_Api.VerifyRequests.Single().OfferId);
        }

        [Test]
        public void Service_OfferIdOfTheGame_IsSentAsGiven()
        {
            CreateService();

            Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions
            {
                GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken, OfferId = "spring-sale",
            }));

            Assert.AreEqual("spring-sale", m_Api.VerifyRequests.Single().OfferId);
        }

        [Test]
        public void Service_OfferReplacingASubscription_NamesTheOwnedOneAndTheMode()
        {
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("premium_yearly", new PurchaseOptions
            {
                GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken,
                ReplacesProductId = "premium",
                ReplacementMode = ReplacementMode.ChargeFullPrice,
            }));

            Assert.AreEqual(PurchaseStatus.Success, result.Status);
            Assert.IsEmpty(m_Store.ReplaceCalls, "not Unity IAP's subscription change");
            var flow = m_Store.GoogleBilling.Flows.Single();
            Assert.AreEqual(("premium_yearly", "premium_android", ReplacementMode.ChargeFullPrice),
                (flow.ProductId, flow.OldProductId, flow.ReplacementMode));
        }

        [Test]
        public void Service_DeferredReplacement_IsPendingUntilTheRenewal()
        {
            CreateService();
            m_Store.GoogleBilling.OnPurchase = _ => @"{""code"":0,""purchases"":[]}";

            var result = Sync.Run(m_Service.PurchaseAsync("premium_yearly", new PurchaseOptions
            {
                GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken,
                ReplacesProductId = "premium",
                ReplacementMode = ReplacementMode.Deferred,
            }));

            Assert.AreEqual(PurchaseStatus.PendingPayment, result.Status);
            Assert.IsEmpty(m_Api.VerifyRequests);
        }

        [Test]
        public void Service_CancelledOfferPurchase_ReleasesTheProduct()
        {
            CreateService();
            m_Store.GoogleBilling.OnPurchase = _ => FakeGooglePlayBillingBridge.Error(1, "");

            var cancelled = Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions { GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken }));
            Assert.AreEqual(PurchaseStatus.Cancelled, cancelled.Status);

            m_Store.GoogleBilling.OnPurchase = flow => FakeGooglePlayBillingBridge.Purchased(flow.ProductId);
            var bought = Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions { GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken }));
            Assert.AreEqual(PurchaseStatus.Success, bought.Status);
        }

        [Test]
        public void Service_Unsupported_ReportsItWithoutBuying()
        {
            m_Store.GoogleBilling.IsAvailable = false;
            CreateService();

            Assert.IsFalse(m_Service.GoogleOffersSupported);
            var e = Assert.Throws<BillingException>(() => Sync.Run(m_Service.GetGoogleOffersAsync("premium")));
            Assert.AreEqual(GooglePlayOfferClient.NotSupportedMessage, e.Message);

            var result = Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions { GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken }));
            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            Assert.AreEqual(GooglePlayOfferClient.NotSupportedMessage, result.Message);
            Assert.IsEmpty(m_Store.GoogleBilling.Flows);
            Assert.IsEmpty(m_Store.PurchaseCalls, "never bought without the offer");
        }

        [Test]
        public void Service_OfferOfAConsumable_Fails()
        {
            CreateService();

            var result = Sync.Run(m_Service.PurchaseAsync("gems_100", new PurchaseOptions { GoogleOfferToken = FakeGooglePlayBillingBridge.IntroToken }));

            Assert.AreEqual(PurchaseStatus.Failed, result.Status);
            StringAssert.Contains("subscriptions only", result.Message);
            Assert.Throws<BillingException>(() => Sync.Run(m_Service.GetGoogleOffersAsync("gems_100")));
            Assert.IsEmpty(m_Store.GoogleBilling.Flows);
        }

        [Test]
        public void Service_PurchaseWithoutAToken_StillGoesThroughUnityIap()
        {
            CreateService();
            m_Store.OnPurchase = id => FakeStore.GoogleOrder(id);

            Sync.Run(m_Service.PurchaseAsync("premium", new PurchaseOptions { OfferId = "intro" }));

            Assert.AreEqual("premium_android", m_Store.PurchaseCalls.Single());
            Assert.IsEmpty(m_Store.GoogleBilling.Flows);
        }
    }
}
