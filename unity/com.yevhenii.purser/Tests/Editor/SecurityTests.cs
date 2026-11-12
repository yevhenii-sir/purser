using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.CompilerServices;
using System.Threading;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    public sealed class UrlPolicyTests
    {
        [Test]
        public void ServerUrl_IsHttps_OrPlainHttpOnLoopback()
        {
            Assert.IsTrue(UrlPolicy.IsAllowedServerUrl("https://maincloud.spacetimedb.com/v1/database/billing/route"));
            Assert.IsTrue(UrlPolicy.IsAllowedServerUrl("HTTPS://billing.example.com/route"));
            Assert.IsTrue(UrlPolicy.IsAllowedServerUrl("http://localhost:3000/v1/database/billing/route"));
            Assert.IsTrue(UrlPolicy.IsAllowedServerUrl("http://127.0.0.1:3013/v1/database/billing/route"));
            Assert.IsTrue(UrlPolicy.IsAllowedServerUrl("http://[::1]:3000/route"));

            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl("http://billing.example.com/route"));
            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl("http://localhost.example.com/route"));
            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl("http://192.168.1.10:3000/route"));
            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl("ftp://billing.example.com/route"));
            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl("billing.example.com/route"));
            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl(""));
            Assert.IsFalse(UrlPolicy.IsAllowedServerUrl(null));
        }

        [Test]
        public void ServerUrlProblem_OnlyOutsideTheEditor()
        {
            StringAssert.Contains("https", UrlPolicy.ServerUrlProblem("http://billing.example.com/route", false));
            Assert.IsNull(UrlPolicy.ServerUrlProblem("http://billing.example.com/route", true));
            Assert.IsNull(UrlPolicy.ServerUrlProblem("https://billing.example.com/route", false));
            Assert.IsNull(UrlPolicy.ServerUrlProblem("http://localhost:3000/route", false));
        }

        [Test]
        public void PaymentPages_MustBeAbsoluteHttps()
        {
            Assert.IsTrue(UrlPolicy.IsHttps("https://checkout.stripe.com/c/pay/cs_test_1"));
            Assert.IsFalse(UrlPolicy.IsHttps("http://checkout.stripe.com/c/pay/cs_test_1"));
            Assert.IsFalse(UrlPolicy.IsHttps("javascript:alert(1)"));
            Assert.IsFalse(UrlPolicy.IsHttps("file:///etc/passwd"));
            Assert.IsFalse(UrlPolicy.IsHttps("/c/pay/cs_test_1"));
            Assert.IsFalse(UrlPolicy.IsHttps(null));
        }

        [Test]
        public void JsonHttp_RefusesAnInsecureServerOutsideTheEditor()
        {
            var transport = new FakeTransport();
            var player = new JsonHttp("http://billing.example.com/route", 30, transport, isEditor: false);
            Assert.Throws<BillingException>(() => Sync.Run(player.PostAsync<StatusResponse>("/entitlements", new { }, CancellationToken.None)));
            Assert.Throws<BillingException>(() => Sync.Run(player.GetAsync("/healthz", CancellationToken.None)));
            Assert.IsEmpty(transport.Requests, "nothing is sent");

            var editor = new JsonHttp("http://billing.example.com/route", 30, transport, isEditor: true);
            Sync.Run(editor.PostAsync<StatusResponse>("/entitlements", new { }, CancellationToken.None));
            var local = new JsonHttp("http://127.0.0.1:3013/route", 30, transport, isEditor: false);
            Sync.Run(local.PostAsync<StatusResponse>("/entitlements", new { }, CancellationToken.None));
            Assert.AreEqual(2, transport.Requests.Count);
        }

        [Test]
        public void BillingService_RefusesToInitializeWithAnInsecureServerOutsideTheEditor()
        {
            var config = BillingConfig.Create("http://billing.example.com/route", new BillingProductConfig("gems_100", BillingProductType.Consumable));
            config.refreshEntitlementsOnInit = false;
            config.logLevel = BillingLogLevel.None;
            BillingService Create(bool isEditor) => new BillingService(config, new BillingDependencies
            {
                Store = new FakeStore(),
                Api = new FakeApi(),
                Storage = new MemoryBillingStorage(),
                IsEditor = isEditor,
            });
            try
            {
                var player = Create(false);
                var error = Assert.Throws<BillingException>(() => Sync.Run(player.InitializeAsync()));
                StringAssert.Contains("https", error.Message);
                Assert.IsFalse(player.IsInitialized);
                player.Dispose();

                var editor = Create(true);
                Sync.Run(editor.InitializeAsync());
                Assert.IsTrue(editor.IsInitialized, "the Editor may use a local http server");
                editor.Dispose();

                config.serverBaseUrl = "http://localhost:3000/v1/database/billing/route";
                var local = Create(false);
                Sync.Run(local.InitializeAsync());
                Assert.IsTrue(local.IsInitialized, "plain http is fine for localhost");
                local.Dispose();
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(config);
            }
        }
    }

    public sealed class EntitlementsClockTests
    {
        private static readonly DateTime T0 = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);
        private static readonly long T0Ms = new DateTimeOffset(T0).ToUnixTimeMilliseconds();

        [Test]
        public void EstimatedServerNow_IgnoresAClockSetBack()
        {
            var e = new Entitlements("u", new Dictionary<string, long> { ["premium"] = T0Ms + 3_600_000 }, null, T0Ms, T0, false);
            Assert.AreEqual(T0Ms + 60_000, e.EstimatedServerNowMs(T0.AddMinutes(1)));
            Assert.AreEqual(T0Ms, e.EstimatedServerNowMs(T0.AddDays(-30)), "time never runs backwards from the server's clock");
            Assert.IsFalse(e.IsSubscriptionActive("premium", T0.AddHours(2)));
        }

        [Test]
        public void IsFresh_ForMaxAge_AndNotWithAClockSetBack()
        {
            var e = new Entitlements("u", null, null, T0Ms, T0, true);
            var week = TimeSpan.FromDays(7);
            Assert.IsTrue(e.IsFresh(T0, week));
            Assert.IsTrue(e.IsFresh(T0.AddDays(6), week));
            Assert.IsFalse(e.IsFresh(T0.AddDays(8), week));
            Assert.IsTrue(e.IsFresh(T0.AddMinutes(-5), week), "small clock corrections are fine");
            Assert.IsFalse(e.IsFresh(T0.AddDays(-2), week), "clock set back before the data was received");
        }

        [Test]
        public void IsActive_StopsTrustingOldSubscriptionData_ButKeepsNonConsumables()
        {
            var now = T0;
            var storage = new MemoryBillingStorage();
            var config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("premium", BillingProductType.Subscription),
                new BillingProductConfig("vip", BillingProductType.NonConsumable));
            config.refreshEntitlementsOnInit = false;
            config.logLevel = BillingLogLevel.None;
            BillingService Create(FakeApi api)
            {
                var service = new BillingService(config, new BillingDependencies
                {
                    Store = new FakeStore(),
                    Api = api,
                    UserIdProvider = new StaticUserIdProvider("u"),
                    Storage = storage,
                    UtcNow = () => now,
                    IsEditor = false,
                });
                Sync.Run(service.InitializeAsync());
                return service;
            }
            try
            {
                var api = new FakeApi { EntitlementsResult = FakeApi.Entitlements(new Dictionary<string, long> { ["premium"] = T0Ms + 30L * 86_400_000 }, "vip") };
                api.EntitlementsResult.Value.ServerTimeMs = T0Ms;
                var online = Create(api);
                Sync.Run(online.RefreshEntitlementsAsync());
                Assert.IsTrue(online.IsActive("premium"));
                now = T0.AddDays(6);
                Assert.IsTrue(online.IsActive("premium"));
                now = T0.AddDays(8);
                Assert.IsFalse(online.IsActive("premium"), "older than entitlementsMaxAgeDays");
                Assert.IsTrue(online.IsActive("vip"));
                now = T0.AddDays(-2);
                Assert.IsFalse(online.IsActive("premium"), "clock set back before the data was received");
                config.entitlementsMaxAgeDays = 0;
                now = T0.AddDays(8);
                Assert.IsTrue(online.IsActive("premium"), "0 = no limit");
                online.Dispose();

                // Next launch, server unreachable: the cached copy follows the same rule.
                config.entitlementsMaxAgeDays = BillingConfig.DefaultEntitlementsMaxAgeDays;
                var offline = Create(new FakeApi());
                Assert.IsTrue(offline.Entitlements.IsFromCache);
                Assert.IsFalse(offline.IsActive("premium"));
                Assert.IsTrue(offline.IsActive("vip"));
                now = T0.AddDays(1);
                Assert.IsTrue(offline.IsActive("premium"));
                offline.Dispose();
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(config);
            }
        }
    }

    public sealed class PackageVersionTests
    {
        [Test]
        public void PurserInfoVersion_MatchesPackageJson()
        {
            var version = UnityEditor.PackageManager.PackageInfo.FindForAssembly(typeof(PurserInfo).Assembly)?.version;
            if (string.IsNullOrEmpty(version))
            {
                // Not imported as a package (e.g. copied into Assets): read the file next to the tests.
                var path = Path.GetFullPath(Path.Combine(Path.GetDirectoryName(ThisFile()) ?? "", "..", "..", "package.json"));
                if (File.Exists(path))
                    version = (string)JObject.Parse(File.ReadAllText(path))["version"];
            }
            if (string.IsNullOrEmpty(version))
                Assert.Inconclusive("package.json of com.yevhenii.purser not found");
            Assert.AreEqual(version, PurserInfo.Version, "update PurserInfo.Version together with package.json");
        }

        private static string ThisFile([CallerFilePath] string path = "") => path;
    }
}
