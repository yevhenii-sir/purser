using System;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Threading.Tasks;
using NUnit.Framework;
using UnityEngine;

namespace Purser.Tests
{
    public sealed class ProfileTests
    {
        private const string Prod = "https://billing.example.com/route";
        private const string Dev = "https://dev.billing.example.com/route";
        private BillingConfig m_Config;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create(Prod,
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("vip", BillingProductType.Subscription)
                    .WithStoreId(BillingStores.Stripe, "price_live_vip").WithStoreId(BillingStores.Apple, "com.test.vip")
                    .WithAccess("premium"));
            m_Config.WithBuild(BillingStores.Google, "com.test.game").WithBuild(BillingStores.Stripe, "com.test.game.web");
            m_Config.profiles.Add(new BillingProfile("dev").WithServer(Dev).WithBuild(BillingStores.Stripe, "com.test.game.web.dev")
                .WithProductId("vip", BillingStores.Stripe, "price_test_vip").AsTest());
            m_Config.profiles.Add(new BillingProfile("prod"));
            m_Config.logLevel = BillingLogLevel.None;
        }

        [TearDown]
        public void TearDown()
        {
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        [Test]
        public void SelectProfile_FollowsTheRules()
        {
            string Pick(string runtime, bool editor, bool debug) => BillingConfig.SelectProfile(runtime, editor, debug, "ed", "dev", "rel");
            Assert.AreEqual("ed", Pick(null, true, true));
            Assert.AreEqual("dev", Pick(null, false, true));
            Assert.AreEqual("rel", Pick(null, false, false));
            Assert.AreEqual("qa", Pick("qa", true, true), "UseProfile wins everywhere");
            Assert.AreEqual("", Pick("", false, false), "\"\" is no profile");
            Assert.AreEqual("", BillingConfig.SelectProfile(null, false, false, "ed", "dev", null));
            Assert.AreEqual("dev", BillingConfig.SelectProfile(" dev ", false, false, "", "", ""));
        }

        [Test]
        public void Resolved_WithoutAProfile_IsTheConfigItself()
        {
            Assert.AreSame(m_Config, m_Config.Resolved());
            Assert.AreEqual("", m_Config.ProfileName);
        }

        [Test]
        public void Resolved_AppliesTheProfile_AndLeavesTheAssetAlone()
        {
            m_Config.editorProfile = "dev";
            var dev = m_Config.Resolved();
            Assert.AreNotSame(m_Config, dev);
            Assert.AreEqual(("dev", "dev"), (m_Config.ProfileName, dev.ProfileName));
            Assert.AreEqual(Dev, dev.serverBaseUrl);
            Assert.AreEqual("com.test.game.web.dev", dev.AppIdFor(BillingStores.Stripe));
            Assert.AreEqual("com.test.game", dev.AppIdFor(BillingStores.Google), "a store the profile does not name comes from the config");
            Assert.AreEqual("price_test_vip", dev.FindById("vip").StoreId(BillingStores.Stripe));
            Assert.AreEqual("com.test.vip", dev.FindById("vip").StoreId(BillingStores.Apple));
            Assert.AreSame(dev, dev.Resolved(), "a resolved copy is final");

            Assert.AreEqual(Prod, m_Config.serverBaseUrl);
            Assert.AreEqual("com.test.game.web", m_Config.AppIdFor(BillingStores.Stripe));
            Assert.AreEqual("price_live_vip", m_Config.FindById("vip").StoreId(BillingStores.Stripe));

            var prod = m_Config.WithProfile(m_Config.FindProfile("prod"));
            Assert.AreEqual((Prod, "com.test.game.web", "price_live_vip"),
                (prod.serverBaseUrl, prod.AppIdFor(BillingStores.Stripe), prod.FindById("vip").StoreId(BillingStores.Stripe)),
                "an empty profile changes nothing");
        }

        [Test]
        public void UseProfile_PicksOneAtRunTime()
        {
            m_Config.editorProfile = "dev";
            m_Config.UseProfile("prod");
            Assert.AreEqual(Prod, m_Config.Resolved().serverBaseUrl);
            m_Config.UseProfile("");
            Assert.AreSame(m_Config, m_Config.Resolved());
            m_Config.UseProfile(null);
            Assert.AreEqual(Dev, m_Config.Resolved().serverBaseUrl);
            m_Config.UseProfile("staging");
            var e = Assert.Throws<BillingException>(() => m_Config.Resolved());
            StringAssert.Contains("staging", e.Message);
            StringAssert.Contains("dev, prod", e.Message);
        }

        [Test]
        public void Clone_CopiesEveryField()
        {
            // A new field of BillingConfig must be copied by Clone (and listed here).
            var fields = typeof(BillingConfig).GetFields(BindingFlags.Public | BindingFlags.Instance).Select(f => f.Name).OrderBy(n => n);
            CollectionAssert.AreEqual(new[]
            {
                "androidStore", "bindPurchasesToUser", "builds", "childDirected", "collectAppleAdsAttribution", "developmentProfile", "editorMode",
                "editorProfile", "entitlementsMaxAgeDays", "failOpenAfterSeconds", "logLevel", "products", "profiles", "refreshEntitlementsOnInit",
                "releaseProfile", "requestTimeoutSeconds", "responseSigningPublicKey", "restoreOnFirstLaunch", "retryDelaysSeconds", "serverBaseUrl",
            }, fields.ToArray());
            m_Config.androidStore = AndroidStore.AmazonAppstore;
            m_Config.bindPurchasesToUser = false;
            m_Config.collectAppleAdsAttribution = true;
            m_Config.editorMode = EditorVerificationMode.CallServer;
            m_Config.entitlementsMaxAgeDays = 3;
            m_Config.failOpenAfterSeconds = 8;
            m_Config.childDirected = true;
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.restoreOnFirstLaunch = false;
            m_Config.requestTimeoutSeconds = 9;
            m_Config.retryDelaysSeconds = new[] { 1, 2 };
            m_Config.developmentProfile = "dev";
            var copy = m_Config.Clone();
            foreach (var f in typeof(BillingConfig).GetFields(BindingFlags.Public | BindingFlags.Instance))
            {
                var (a, b) = (f.GetValue(m_Config), f.GetValue(copy));
                if (a is System.Collections.IEnumerable list && !(a is string))
                    Assert.AreEqual(list.Cast<object>().Count(), ((System.Collections.IEnumerable)b).Cast<object>().Count(), f.Name);
                else
                    Assert.AreEqual(a, b, f.Name);
            }
            copy.FindById("vip").WithStoreId(BillingStores.Stripe, "changed");
            copy.retryDelaysSeconds[0] = 99;
            Assert.AreEqual("price_live_vip", m_Config.FindById("vip").StoreId(BillingStores.Stripe), "products are deep copies");
            Assert.AreEqual(1, m_Config.retryDelaysSeconds[0]);
        }

        [Test]
        public void TheServiceAndTheAdapters_RunWithTheProfile()
        {
            m_Config.editorProfile = "dev";
            var stripe = new FakeStripeApi();
            var adapter = new StripeStoreAdapter(m_Config, new StripeCheckoutOptions
            {
                SuccessUrl = "https://game.example/ok", CancelUrl = "https://game.example/",
                OpenUrl = _ => { },
            }, stripe, new MemoryBillingStorage());
            using (var service = new BillingService(m_Config, new BillingDependencies
                   {
                       Store = adapter, Api = new FakeApi(), Storage = new MemoryBillingStorage(),
                       UserIdProvider = new StaticUserIdProvider("player-1"), Delay = (d, ct) => Task.CompletedTask,
                       IsEditor = false, OpenUrl = _ => { },
                   }))
            {
                Sync.Run(service.InitializeAsync());
                Assert.AreEqual("dev", service.Profile);
                service.PurchaseAsync("vip");
                var checkout = stripe.Checkouts.Single();
                Assert.AreEqual(("com.test.game.web.dev", "price_test_vip"), (checkout.AppId, checkout.ProductId),
                    "the adapter made from the asset uses the dev build and the dev product id");
            }

            var transport = new FakeTransport();
            var api = new BillingApiClient(m_Config, transport);
            Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { AppId = "a", UserId = "player-1" }));
            StringAssert.StartsWith(Dev + "/", transport.Requests.Single().Url);
        }

        [Test]
        public void CacheScope_DependsOnTheServer()
        {
            var prod = BillingContext.CacheScopeOf("com.test.game", Prod);
            Assert.AreNotEqual(prod, BillingContext.CacheScopeOf("com.test.game", Dev));
            Assert.AreEqual(prod, BillingContext.CacheScopeOf("com.test.game", Prod + "/"));
            StringAssert.StartsWith("com.test.game@", prod);
        }

        [Test]
        public void Validate_ChecksTheProfiles()
        {
            Assert.IsEmpty(m_Config.Validate());
            m_Config.releaseProfile = "dev";
            m_Config.developmentProfile = "missing";
            m_Config.profiles.Add(new BillingProfile("prod"));
            m_Config.profiles.Add(new BillingProfile("qa").WithServer("http://qa.example.com/route").WithProductId("nope", BillingStores.Stripe, "x")
                .WithBuild("store9", "a"));
            var problems = m_Config.Validate();
            void Has(string text) => Assert.IsTrue(problems.Any(p => p.Contains(text)), $"'{text}' in {string.Join(" | ", problems)}");
            Has("Two profiles named 'prod'");
            Has("Development profile 'missing' does not exist");
            Has("release profile 'dev' is a test environment");
            Has("Profile 'qa': server base URL must use https://");
            Has("Profile 'qa': no product 'nope'");
            Has("Profile 'qa': unknown store 'store9'");
        }

        [Test]
        public void Validate_SkipsTheConfigUrl_WhenEveryProfileHasItsOwn()
        {
            m_Config.serverBaseUrl = BillingConfig.DefaultServerUrl;
            Assert.IsTrue(m_Config.Validate().Any(p => p.Contains(BillingConfig.ServerUrlPlaceholder)));
            m_Config.FindProfile("prod").WithServer(Prod);
            m_Config.editorProfile = m_Config.developmentProfile = "dev";
            m_Config.releaseProfile = "prod";
            Assert.IsFalse(m_Config.Validate().Any(p => p.Contains(BillingConfig.ServerUrlPlaceholder)), string.Join(" | ", m_Config.Validate()));
        }

        [Test]
        public void PasteIntoAProfile_KeepsTheEnvironmentInTheProfile_AndTheCatalogShared()
        {
            const string devAdmin = @"{
                ""serverBaseUrl"": ""https://dev2.billing.example.com/route"",
                ""builds"": [ { ""store"": ""google"", ""appId"": ""com.test.game"" }, { ""store"": ""apple"", ""appId"": ""com.test.game.ios.dev"" } ],
                ""products"": [
                    { ""id"": ""vip"", ""type"": ""subscription"", ""storeIds"": { ""stripe"": ""price_test_vip2"", ""apple"": ""com.test.vip"" }, ""access"": [""premium"", ""no_ads""] },
                    { ""id"": ""coins"", ""type"": ""consumable"", ""storeIds"": { ""stripe"": ""price_test_coins"" } }
                ]
            }";
            var preview = m_Config.PreviewJsonForProfile(devAdmin, "dev");
            Assert.AreEqual(Dev, m_Config.FindProfile("dev").serverBaseUrl, "a preview changes nothing");
            Assert.IsNull(m_Config.FindById("coins"));

            var changes = m_Config.MergeJsonIntoProfile(devAdmin, "dev");
            CollectionAssert.AreEqual(preview.Select(c => c.Target + c.Details), changes.Select(c => c.Target + c.Details));
            var dev = m_Config.FindProfile("dev");
            Assert.AreEqual("https://dev2.billing.example.com/route", dev.serverBaseUrl);
            Assert.AreEqual(Prod, m_Config.serverBaseUrl, "the config keeps its server");
            CollectionAssert.AreEquivalent(new[] { "stripe com.test.game.web.dev", "apple com.test.game.ios.dev" },
                dev.builds.Select(b => b.store + " " + b.appId), "a build equal to the config's needs no profile row");
            Assert.AreEqual("price_test_vip2", dev.productIds.Single(o => o.product == "vip").id);
            Assert.AreEqual("price_live_vip", m_Config.FindById("vip").StoreId(BillingStores.Stripe), "the config keeps its ids");
            CollectionAssert.AreEqual(new[] { "premium", "no_ads" }, m_Config.FindById("vip").AccessLevels(), "the catalog is shared");
            Assert.AreEqual("price_test_coins", m_Config.FindById("coins").StoreId(BillingStores.Stripe), "a new product joins the config");
            Assert.IsEmpty(m_Config.PreviewJsonForProfile(devAdmin, "dev"), "pasting again changes nothing");

            // The same ids as the config: the profile rows go away.
            const string sameAsConfig = @"{ ""builds"": [ { ""store"": ""stripe"", ""appId"": ""com.test.game.web"" } ],
                ""products"": [ { ""id"": ""vip"", ""type"": ""subscription"", ""storeIds"": { ""stripe"": ""price_live_vip"" } } ] }";
            m_Config.MergeJsonIntoProfile(sameAsConfig, "dev");
            Assert.IsFalse(dev.builds.Any(b => b.store == BillingStores.Stripe));
            Assert.IsFalse(dev.productIds.Any(o => o.product == "vip"));
            Assert.Throws<ArgumentException>(() => m_Config.PreviewJsonForProfile(devAdmin, "nope"));
        }
    }
}
