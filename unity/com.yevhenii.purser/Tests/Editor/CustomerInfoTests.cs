using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>A billing service on fakes with a movable clock (Google Play unless the store is changed).</summary>
    internal sealed class ServiceHarness : IDisposable
    {
        public const string AppId = "com.test.game";
        public readonly BillingConfig Config;
        public readonly FakeStore Store = new FakeStore();
        public readonly FakeApi Api = new FakeApi();
        public IBillingStorage Storage = new MemoryBillingStorage();
        public IUserIdProvider Users = new StaticUserIdProvider("player-1");
        public IPlayerTokenProvider Tokens;
        public DateTime Now = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);
        public readonly List<TimeSpan> Delays = new List<TimeSpan>();
        /// <summary>Replaces the immediate delay (e.g. a gate the test opens).</summary>
        public Func<TimeSpan, CancellationToken, Task> Delay;
        public bool IsEditor;
        public BillingService Service;
        public readonly List<PurchaseResult> Succeeded = new List<PurchaseResult>();
        public readonly List<PurchaseResult> Pending = new List<PurchaseResult>();
        public readonly List<PurchaseResult> Failed = new List<PurchaseResult>();
        public readonly List<CustomerInfo> Changes = new List<CustomerInfo>();

        public long NowMs => new DateTimeOffset(Now).ToUnixTimeMilliseconds();

        public ServiceHarness()
        {
            Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("no_ads", BillingProductType.NonConsumable).WithAccess("no_ads"),
                new BillingProductConfig("premium", BillingProductType.Subscription)
                    .WithStoreId(BillingStores.Google, "premium_android").WithStoreId(BillingStores.Apple, "premium_ios")
                    .WithAccess("premium", "no_ads"),
                new BillingProductConfig("premium_year", BillingProductType.Subscription).WithAccess("premium"));
            Config.WithBuild(BillingStores.Google, AppId).WithBuild(BillingStores.Apple, "com.test.game.ios");
            Config.retryDelaysSeconds = new[] { 5, 30 };
            Config.refreshEntitlementsOnInit = false;
            Config.logLevel = BillingLogLevel.None;
        }

        public BillingService Start(bool initialize = true)
        {
            Service = new BillingService(Config, new BillingDependencies
            {
                Store = Store,
                Api = Api,
                UserIdProvider = Users,
                PlayerTokenProvider = Tokens,
                Storage = Storage,
                UtcNow = () => Now,
                Delay = (delay, ct) =>
                {
                    Delays.Add(delay);
                    return Delay?.Invoke(delay, ct) ?? Task.CompletedTask;
                },
                IsEditor = IsEditor,
                CreateLifecycleHook = false,
            });
            Service.PurchaseSucceeded += Succeeded.Add;
            Service.PurchasePending += Pending.Add;
            Service.PurchaseFailed += Failed.Add;
            Service.CustomerInfoChanged += Changes.Add;
            if (initialize)
                Sync.Run(Service.InitializeAsync());
            return Service;
        }

        /// <summary>The next <c>/entitlements</c> answer: subscriptions and access levels expiring in
        /// <paramref name="days"/> (0 = lifetime), non-consumables.</summary>
        public void Answer(IDictionary<string, double> subscriptionDays, IDictionary<string, double> accessDays, params string[] nonConsumables)
        {
            long Expiry(double days) => days == 0 ? 0 : new DateTimeOffset(Now.AddDays(days)).ToUnixTimeMilliseconds();
            var dto = FakeApi.Info(
                subscriptionDays?.ToDictionary(kv => kv.Key, kv => Expiry(kv.Value)),
                accessDays?.ToDictionary(kv => kv.Key, kv => Expiry(kv.Value)),
                nonConsumables);
            dto.ServerTimeMs = NowMs;
            Api.EntitlementsResult = new ApiResult<CustomerInfoDto>(200, dto, null);
        }

        public void Dispose()
        {
            Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(Config);
        }
    }

    public sealed class CustomerInfoTests
    {
        [Test]
        public void HasAccess_ChecksTheServersAccessLevels()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Answer(new Dictionary<string, double> { ["premium"] = 10 }, new Dictionary<string, double> { ["premium"] = 10, ["no_ads"] = 0 });
                var info = Sync.Run(h.Service.RefreshCustomerInfoAsync());

                Assert.IsTrue(h.Service.HasAccess("premium"));
                Assert.IsTrue(h.Service.HasAccess("no_ads"));
                Assert.IsFalse(h.Service.HasAccess("vip"));
                Assert.IsFalse(h.Service.HasAccess(null));
                Assert.IsFalse(h.Service.HasAccess(""));
                Assert.AreSame(info, h.Service.CustomerInfo);
                Assert.AreEqual(h.Now.AddDays(10), info.Access["premium"].ExpiresAt);
                Assert.IsTrue(info.Subscriptions["premium"].WillRenew);

                h.Config.entitlementsMaxAgeDays = 0;
                h.Now = h.Now.AddDays(11);
                Assert.IsFalse(h.Service.HasAccess("premium"), "expired by the server's clock");
                Assert.IsTrue(h.Service.HasAccess("no_ads"), "lifetime access does not expire");
            }
        }

        [Test]
        public void StaleCustomerInfo_IsNotTrusted_ForExpiringItems_IncludingGetExpiration()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Answer(new Dictionary<string, double> { ["premium"] = 30 }, new Dictionary<string, double> { ["premium"] = 30, ["no_ads"] = 0 }, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                var expires = h.Now.AddDays(30);
                Assert.AreEqual(expires, h.Service.GetExpiration("premium"));

                h.Now = h.Now.AddDays(8); // older than entitlementsMaxAgeDays (7)
                Assert.IsFalse(h.Service.IsActive("premium"));
                Assert.IsFalse(h.Service.HasAccess("premium"));
                Assert.IsNull(h.Service.GetExpiration("premium"), "the same limit as IsActive");
                Assert.IsTrue(h.Service.IsActive("no_ads"));
                Assert.IsTrue(h.Service.HasAccess("no_ads"));

                h.Config.entitlementsMaxAgeDays = 0;
                Assert.AreEqual(expires, h.Service.GetExpiration("premium"), "0 = no limit");
                Assert.AreEqual(expires, h.Service.GetExpiration("premium_android"), "by a store id too");
            }
        }

        [Test]
        public void CustomerInfoChanged_FiresOnlyWhenWhatItGivesChanged()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Changes.Clear();
                h.Answer(new Dictionary<string, double> { ["premium"] = 10 }, new Dictionary<string, double> { ["premium"] = 10 });
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(1, h.Changes.Count);

                h.Api.EntitlementsResult.Value.ServerTimeMs += 60_000; // same items, later answer
                h.Now = h.Now.AddMinutes(1);
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(1, h.Changes.Count, "a refresh with the same items is not a change");
                Assert.AreEqual(h.Now, h.Service.CustomerInfo.FetchedAtUtc, "the fetch time is still updated");

                h.Api.EntitlementsResult.Value.Subscriptions["premium"].BillingIssue = true;
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(2, h.Changes.Count);
                Assert.IsTrue(h.Changes.Last().Subscriptions["premium"].HasBillingIssue);

                h.Api.EntitlementsResult = new ApiResult<CustomerInfoDto>(0, null, "offline");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(2, h.Changes.Count, "a failed refresh changes nothing");

                h.Service.SetUserId("player-2");
                Assert.AreEqual(3, h.Changes.Count, "another player");
                Assert.IsEmpty(h.Changes.Last().Subscriptions, "nothing cached for player-2");
            }
        }

        [Test]
        public void CustomerInfoChanged_HandlerFailure_IsLogged()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Service.CustomerInfoChanged += _ => throw new InvalidOperationException("ui bug");
                h.Answer(null, new Dictionary<string, double> { ["premium"] = 10 });
                Assert.DoesNotThrow(() => Sync.Run(h.Service.RefreshCustomerInfoAsync()));
                Assert.IsTrue(h.Service.HasAccess("premium"));
            }
        }

        [Test]
        public void Cache_UsesTheNewKey_AndIgnoresTheOldFormat()
        {
            using (var h = new ServiceHarness())
            {
                var storage = new MemoryBillingStorage();
                h.Storage = storage;
                storage.Save("purser.entitlements." + ServiceHarness.AppId + ".player-1",
                    "{\"dto\":{\"subscriptions\":{\"premium\":1900000000000},\"non_consumables\":[\"no_ads\"]},\"fetched_at_ticks\":1}");
                h.Start();
                Assert.IsEmpty(h.Service.CustomerInfo.NonConsumables, "the old cache is not migrated");

                h.Answer(null, new Dictionary<string, double> { ["no_ads"] = 0 }, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                var saved = JObject.Parse(storage.Load("purser.customer_info." + ServiceHarness.AppId + ".player-1"));
                Assert.AreEqual("no_ads", (string)saved["dto"]["non_consumables"][0]);
                Assert.AreEqual(0, (long)saved["dto"]["access"]["no_ads"]["expires_at_ms"]);

                // Next launch offline: the cached access levels are there.
                h.Service.Dispose();
                h.Api.EntitlementsResult = new ApiResult<CustomerInfoDto>(0, null, "offline");
                h.Start();
                Assert.IsTrue(h.Service.CustomerInfo.IsFromCache);
                Assert.IsTrue(h.Service.HasAccess("no_ads"));
            }
        }

        [Test]
        public void Editor_LocalPurchases_GrantTheConfigsAccessLevels_ForTheNextSessionToo()
        {
            using (var h = new ServiceHarness { IsEditor = true })
            {
                h.Start();
                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id);
                var result = Sync.Run(h.Service.PurchaseAsync("premium"));

                Assert.AreEqual(PurchaseStatus.Success, result.Status);
                Assert.IsEmpty(h.Api.VerifyRequests);
                Assert.IsTrue(h.Service.HasAccess("premium"));
                Assert.IsTrue(h.Service.HasAccess("no_ads"));
                Assert.IsTrue(h.Service.IsActive("premium"));
                Assert.AreEqual(h.Now.AddDays(30), h.Service.GetExpiration("premium"));
                Assert.AreEqual("premium", h.Service.CustomerInfo.Access["no_ads"].ProductId);

                h.Store.OnPurchase = id => FakeStore.GoogleOrder(id, "tok-2", "GPA.2");
                Sync.Run(h.Service.PurchaseAsync("no_ads"));
                Assert.IsNull(h.Service.CustomerInfo.Access["no_ads"].ExpiresAt, "the non-consumable gives lifetime access");

                h.Service.Dispose();
                h.Start();
                Assert.IsTrue(h.Service.HasAccess("premium"), "kept for the next Editor session");
                Assert.IsTrue(h.Service.IsActive("no_ads"));
            }
        }

        [Test]
        public void CustomerInfo_PureChecks()
        {
            var t0 = new DateTime(2027, 1, 1, 0, 0, 0, DateTimeKind.Utc);
            var t0Ms = new DateTimeOffset(t0).ToUnixTimeMilliseconds();
            var info = new CustomerInfo("u",
                new Dictionary<string, AccessInfo> { ["premium"] = new AccessInfo("premium", "apple", t0Ms + 60_000, isTrial: true) },
                new Dictionary<string, SubscriptionInfo> { ["premium"] = new SubscriptionInfo("premium", "apple", t0Ms + 60_000, renewsTo: "premium_week") },
                new[] { "no_ads" }, t0Ms, t0, false);
            Assert.IsTrue(info.HasAccess("premium", t0.AddSeconds(30)));
            Assert.IsFalse(info.HasAccess("premium", t0.AddMinutes(2)));
            Assert.IsTrue(info.Access["premium"].IsTrial);
            Assert.AreEqual("premium_week", info.Subscriptions["premium"].RenewsTo);
            Assert.IsTrue(info.Owns("no_ads"));
            Assert.IsFalse(CustomerInfo.Empty.HasAccess("premium", t0));
            Assert.IsTrue(info.HasSameContent(info.AsCached()));
            Assert.IsFalse(info.HasSameContent(CustomerInfo.Empty));
        }
    }

    public sealed class ConfigAccessTests
    {
        [Test]
        public void MergeJson_ReadsAccessLevels_AndToJsonWritesThem()
        {
            var config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("premium", BillingProductType.Subscription).WithAccess("premium"),
                new BillingProductConfig("gems_100", BillingProductType.Consumable));
            try
            {
                const string json = "{\"products\":[" +
                                    "{\"id\":\"premium\",\"type\":\"subscription\",\"access\":[\"premium\",\"no_ads\"]}," +
                                    "{\"id\":\"gems_100\",\"type\":\"consumable\"}," +
                                    "{\"id\":\"no_ads\",\"type\":\"non_consumable\",\"access\":[\"no_ads\"]}]}";
                var preview = config.PreviewJson(json);
                Assert.That(preview.Select(c => c.ToString()), Has.Some.Contains("access premium → premium, no_ads"));
                CollectionAssert.AreEqual(new[] { "premium" }, config.FindById("premium").AccessLevels(), "a preview changes nothing");

                config.MergeJson(json);
                CollectionAssert.AreEqual(new[] { "premium", "no_ads" }, config.FindById("premium").AccessLevels());
                Assert.IsEmpty(config.FindById("gems_100").AccessLevels(), "no access key: kept as it was");
                CollectionAssert.AreEqual(new[] { "no_ads" }, config.FindById("no_ads").AccessLevels());

                var exported = JObject.Parse(config.ToJson());
                var premium = exported["products"].First(p => (string)p["id"] == "premium");
                CollectionAssert.AreEqual(new[] { "premium", "no_ads" }, premium["access"].Select(l => (string)l));
                Assert.IsNull(exported["products"].First(p => (string)p["id"] == "gems_100")["access"]);

                Assert.Throws<FormatException>(() => config.MergeJson("{\"products\":[{\"id\":\"x\",\"type\":\"consumable\",\"access\":\"premium\"}]}"));
                Assert.Throws<FormatException>(() => config.MergeJson("{\"products\":[{\"id\":\"x\",\"type\":\"consumable\",\"access\":[1]}]}"));
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(config);
            }
        }

        [Test]
        public void Validate_ReportsABadSigningKey()
        {
            var config = BillingConfig.Create("https://billing.example.com/route", new BillingProductConfig("a", BillingProductType.Consumable));
            try
            {
                config.responseSigningPublicKey = "not-a-key";
                Assert.That(config.Validate(), Has.Some.Contains("Response signing public key"));
                config.responseSigningPublicKey = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
                Assert.That(config.Validate(), Has.None.Contains("Response signing public key"));
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(config);
            }
        }
    }
}
