using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>Counts what reaches the storage, per key.</summary>
    internal sealed class CountingStorage : IFlushableBillingStorage
    {
        private readonly Dictionary<string, string> m_Values = new Dictionary<string, string>();
        public readonly List<string> Loads = new List<string>();
        public readonly List<string> Saves = new List<string>();
        public int Flushes;

        public string Load(string key)
        {
            Loads.Add(key);
            return m_Values.TryGetValue(key, out var v) ? v : null;
        }

        public void Save(string key, string value)
        {
            Saves.Add(key);
            m_Values[key] = value;
        }

        public void Flush() => Flushes++;

        /// <summary>The value without counting a load.</summary>
        public string Peek(string key) => m_Values.TryGetValue(key, out var v) ? v : null;

        public void Poke(string key, string value) => m_Values[key] = value;
    }

    /// <summary>Ed25519 through both paths (in place and on a thread of the pool), with RFC 8032 section 7.1 vectors.</summary>
    public sealed class Ed25519PathsTests
    {
        /// <summary>Public key, message, signature (hex): TEST 1, TEST 2, TEST 3 and TEST SHA(abc).</summary>
        private static readonly (string Key, string Message, string Signature)[] Vectors =
        {
            ("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
            ("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72",
                "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
            ("fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025", "af82",
                "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a"),
            ("ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
                "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704"),
        };

        private static byte[] Hex(string hex) =>
            Enumerable.Range(0, hex.Length / 2).Select(i => Convert.ToByte(hex.Substring(i * 2, 2), 16)).ToArray();

        [Test]
        public void Rfc8032Vectors_VerifyInPlaceAndOffTheMainThread()
        {
            foreach (var (key, message, signature) in Vectors)
            {
                var (k, m, s) = (Hex(key), Hex(message), Hex(signature));
                Assert.IsTrue(Ed25519.Verify(k, m, s), key);
                var inPlace = Ed25519.VerifyAsync(k, m, s, offThread: false);
                Assert.IsTrue(inPlace.IsCompleted, "in place: done when the call returns");
                Assert.IsTrue(inPlace.Result);
                Assert.IsTrue(Ed25519.VerifyAsync(k, m, s, offThread: true).Result, key + " on a pool thread");

                // The same signature over another message, through both paths.
                var other = m.Concat(new byte[] { 1 }).ToArray();
                Assert.IsFalse(Ed25519.Verify(k, other, s));
                Assert.IsFalse(Ed25519.VerifyAsync(k, other, s, offThread: true).Result);
            }
        }

        [Test]
        public void DecodedKey_IsReused_AndKeysDoNotMixUp()
        {
            var first = Ed25519.DecodeKey(Hex(Vectors[0].Key));
            Assert.AreSame(first, Ed25519.DecodeKey(Hex(Vectors[0].Key)), "the same key is not decoded again");
            Assert.IsTrue(Ed25519.VerifyWith(first, Hex(Vectors[0].Message), Hex(Vectors[0].Signature)));

            // Another key in between, then the first one again.
            Assert.IsTrue(Ed25519.Verify(Hex(Vectors[1].Key), Hex(Vectors[1].Message), Hex(Vectors[1].Signature)));
            Assert.IsFalse(Ed25519.Verify(Hex(Vectors[1].Key), Hex(Vectors[0].Message), Hex(Vectors[0].Signature)));
            Assert.IsTrue(Ed25519.Verify(Hex(Vectors[0].Key), Hex(Vectors[0].Message), Hex(Vectors[0].Signature)));
            Assert.IsNull(Ed25519.DecodeKey(new byte[31]));
        }

        [Test]
        public void ResponseSignature_OffTheMainThread_GivesTheSameAnswer()
        {
            var key = Convert.ToBase64String(Hex(Vectors[1].Key));
            var signature = Convert.ToBase64String(Hex(Vectors[1].Signature));
            Assert.IsNull(ResponseSignature.ProblemAsync(key, Hex("72"), signature, offThread: true).Result);
            StringAssert.Contains("invalid", ResponseSignature.ProblemAsync(key, Hex("73"), signature, offThread: true).Result);
            Assert.IsTrue(ResponseSignature.ProblemAsync(key, Hex("72"), signature, offThread: false).IsCompleted);
        }

        [Test]
        public void Verification_RoughTiming()
        {
            var (k, m, s) = (Hex(Vectors[3].Key), Hex(Vectors[3].Message), Hex(Vectors[3].Signature));
            Assert.IsTrue(Ed25519.Verify(k, m, s)); // warm up, decode the key
            const int runs = 20;
            var watch = Stopwatch.StartNew();
            for (var i = 0; i < runs; i++)
                Assert.IsTrue(Ed25519.Verify(k, m, s));
            watch.Stop();
            UnityEngine.Debug.Log($"[Purser] Ed25519 verification: {watch.Elapsed.TotalMilliseconds / runs:0.00} ms each ({runs} runs)");
        }
    }

    /// <summary>The customer info cache with signed responses: kept as the signed bytes, checked again when read.</summary>
    public sealed class SignedCacheTests
    {
        /// <summary>RFC 8032 TEST 1 public key, base64; the answers below are signed with its secret key over these
        /// exact UTF-8 bytes (RFC 8032 reference implementation).</summary>
        private const string PublicKey = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

        private const string EntitlementsBody =
            "{\"user_id\":\"u1\",\"game_key\":\"game\",\"server_time_ms\":1800000000000,\"access\":{\"premium\":{\"product_id\":\"premium\"," +
            "\"store\":\"google\",\"expires_at_ms\":1900000000000,\"purchased_at_ms\":1790000000000,\"will_renew\":true,\"in_grace\":false," +
            "\"billing_issue\":false,\"trial\":false}},\"subscriptions\":{\"premium\":{\"product_id\":\"premium\",\"store\":\"google\"," +
            "\"expires_at_ms\":1900000000000,\"purchased_at_ms\":1790000000000,\"will_renew\":true,\"in_grace\":false,\"billing_issue\":false," +
            "\"trial\":false}},\"non_consumables\":[\"vip\"]}";
        private const string EntitlementsSignature =
            "SXw+Ve+Kqwe+QI7vM1gvSxcwapIrV0tSxOKgck/5/6VkS6EQPmt00+dW7ZhviOM4xkqlmuMMFNmAtg/a1AA7Cg==";

        private const string RestoreBody =
            "{\"results\":[],\"entitlements\":{\"user_id\":\"u1\",\"game_key\":\"game\",\"server_time_ms\":1800000000000,\"access\":{}," +
            "\"subscriptions\":{},\"non_consumables\":[\"vip\"]}}";
        private const string RestoreSignature =
            "OtZrT9iCaFssjJHbm2OYMdCB6jGjbR/XBd0fD7qhDHcvk9ADw7pzCRlvoGajM3cozI+RJQZ/qsN8lw8xCWSCCQ==";

        /// <summary>Player u2: a family-shared subscription and a paused one.</summary>
        private const string SecondPlayerBody =
            "{\"user_id\":\"u2\",\"game_key\":\"game\",\"server_time_ms\":1800000000000,\"access\":{\"premium\":{\"product_id\":\"premium\"," +
            "\"store\":\"apple\",\"expires_at_ms\":1900000000000,\"purchased_at_ms\":1790000000000,\"will_renew\":true,\"in_grace\":false," +
            "\"billing_issue\":false,\"trial\":false,\"family_shared\":true}},\"subscriptions\":{\"premium\":{\"product_id\":\"premium\",\"store\":\"apple\"," +
            "\"expires_at_ms\":1900000000000,\"purchased_at_ms\":1790000000000,\"will_renew\":true,\"in_grace\":false,\"billing_issue\":false," +
            "\"trial\":false,\"family_shared\":true}},\"non_consumables\":[],\"paused\":{\"premium_year\":{\"product_id\":\"premium_year\",\"store\":\"google\"," +
            "\"resumes_at_ms\":1810000000000}}}";
        private const string SecondPlayerSignature =
            "ex2mJAVOR+f4HaRKA/ZGt1OmWZRIZavpHZ/elCzDEorQ9//i16AkLpSKEYQkGDI7NHX1aynWtqm3itS9Vrr2CQ==";

        private static readonly HttpResponse Offline = new HttpResponse(0, null, "offline");

        private BillingConfig m_Config;
        private FakeTransport m_Transport;
        private FakeStore m_Store;
        private CountingStorage m_Storage;
        private BillingService m_Service;
        private string m_User;
        private readonly DateTime m_Now = DateTimeOffset.FromUnixTimeMilliseconds(1_800_000_000_000).UtcDateTime;

        private string CacheKey => "purser.customer_info." + BillingContext.CacheScopeOf(m_Config.AppIdFor(BillingStores.Google), m_Config.serverBaseUrl) + "." + m_User;

        private static HttpResponse Signed(string body, string signature) =>
            new HttpResponse(200, body, null, new Dictionary<string, string> { ["x-purser-signature"] = signature });

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("premium", BillingProductType.Subscription).WithAccess("premium"),
                new BillingProductConfig("premium_year", BillingProductType.Subscription).WithAccess("premium"),
                new BillingProductConfig("vip", BillingProductType.NonConsumable));
            m_Config.refreshEntitlementsOnInit = false;
            m_Config.logLevel = BillingLogLevel.None;
            m_Config.responseSigningPublicKey = PublicKey;
            m_Transport = new FakeTransport();
            m_Storage = new CountingStorage();
            m_User = "u1";
        }

        [TearDown]
        public void TearDown()
        {
            m_Service?.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        /// <summary>A new launch on the same device (the storage stays).</summary>
        private BillingService Start()
        {
            m_Service?.Dispose();
            m_Store = new FakeStore();
            m_Service = new BillingService(m_Config, new BillingDependencies
            {
                Store = m_Store,
                Api = new BillingApiClient(m_Config, m_Transport),
                UserIdProvider = new StaticUserIdProvider(m_User),
                Storage = m_Storage,
                UtcNow = () => m_Now,
                Delay = (d, ct) => Task.CompletedTask,
                IsEditor = false,
                CreateLifecycleHook = false,
            });
            Sync.Run(m_Service.InitializeAsync());
            return m_Service;
        }

        [Test]
        public void SignedAnswer_IsCachedWithItsSignature_AndCheckedAtTheNextStart()
        {
            m_Transport.Next = Signed(EntitlementsBody, EntitlementsSignature);
            Start();
            Sync.Run(m_Service.RefreshCustomerInfoAsync());
            Assert.IsTrue(m_Service.HasAccess("premium"));

            var saved = JObject.Parse(m_Storage.Peek(CacheKey));
            Assert.AreEqual(EntitlementsBody, Encoding.UTF8.GetString(Convert.FromBase64String((string)saved["body"])));
            Assert.AreEqual(EntitlementsSignature, (string)saved["signature"]);
            Assert.IsNull(saved["dto"], "the customer info is read from the signed bytes only");

            m_Transport.Next = Offline;
            Start();
            Assert.IsTrue(m_Service.CustomerInfo.IsFromCache);
            Assert.IsTrue(m_Service.HasAccess("premium"));
            Assert.IsTrue(m_Service.IsActive("vip"));
        }

        [Test]
        public void EditedCache_IsIgnored()
        {
            m_Transport.Next = Signed(EntitlementsBody, EntitlementsSignature);
            Start();
            Sync.Run(m_Service.RefreshCustomerInfoAsync());
            var genuine = m_Storage.Peek(CacheKey);
            m_Transport.Next = Offline;

            // The expiry pushed out, the signature kept.
            var edited = JObject.Parse(genuine);
            edited["body"] = Convert.ToBase64String(Encoding.UTF8.GetBytes(EntitlementsBody.Replace("1900000000000", "1999999999999")));
            m_Storage.Poke(CacheKey, edited.ToString(Formatting.None));
            Start();
            Assert.IsFalse(m_Service.HasAccess("premium"));
            Assert.IsFalse(m_Service.IsActive("vip"));
            Assert.IsNull(m_Service.CustomerInfo.UserId, "nothing is shown");

            // An unsigned copy (the format of builds without signed responses).
            m_Storage.Poke(CacheKey, "{\"dto\":" + EntitlementsBody + ",\"fetched_at_ticks\":" + m_Now.Ticks + "}");
            Start();
            Assert.IsFalse(m_Service.HasAccess("premium"));

            // Another player's genuine answer under this player's key.
            var borrowed = JObject.Parse(genuine);
            borrowed["body"] = Convert.ToBase64String(Encoding.UTF8.GetBytes(SecondPlayerBody));
            borrowed["signature"] = SecondPlayerSignature;
            m_Storage.Poke(CacheKey, borrowed.ToString(Formatting.None));
            Start();
            Assert.IsFalse(m_Service.HasAccess("premium"));

            // The genuine copy still loads.
            m_Storage.Poke(CacheKey, genuine);
            Start();
            Assert.IsTrue(m_Service.HasAccess("premium"));
        }

        [Test]
        public void RestoreAnswer_IsCachedWithItsSignature()
        {
            m_Transport.Next = Signed(RestoreBody, RestoreSignature);
            Start();
            m_Store.Owned.Add(FakeStore.GoogleOrder("vip"));
            Assert.IsTrue(Sync.Run(m_Service.RestoreAsync()).Owns("vip"));

            var saved = JObject.Parse(m_Storage.Peek(CacheKey));
            Assert.AreEqual(true, (bool)saved["restore"]);
            Assert.AreEqual(RestoreSignature, (string)saved["signature"]);

            m_Transport.Next = Offline;
            Start();
            Assert.IsTrue(m_Service.IsActive("vip"), "read from the entitlements of the signed /restore answer");
        }

        [Test]
        public void SignedAnswer_WithPausedAndFamilyShared_SurvivesTheCache()
        {
            m_User = "u2";
            m_Transport.Next = Signed(SecondPlayerBody, SecondPlayerSignature);
            Start();
            Sync.Run(m_Service.RefreshCustomerInfoAsync());
            m_Transport.Next = Offline;
            Start();

            var info = m_Service.CustomerInfo;
            Assert.IsTrue(info.IsFromCache);
            Assert.IsTrue(info.Subscriptions["premium"].IsFamilyShared);
            Assert.IsTrue(info.Access["premium"].IsFamilyShared);
            Assert.AreEqual("google", info.Paused["premium_year"].Store);
            Assert.AreEqual(DateTimeOffset.FromUnixTimeMilliseconds(1_810_000_000_000).UtcDateTime, info.Paused["premium_year"].ResumesAt);
            Assert.IsFalse(m_Service.IsActive("premium_year"), "a paused subscription gives nothing");
            Assert.IsTrue(m_Service.IsActive("premium"));
        }

        [Test]
        public void Startup_ReadsTheCachedCustomerInfoOnce()
        {
            m_Transport.Next = Signed(EntitlementsBody, EntitlementsSignature);
            Start();
            Sync.Run(m_Service.RefreshCustomerInfoAsync());
            m_Storage.Loads.Clear();

            Start();

            Assert.AreEqual(1, m_Storage.Loads.Count(k => k == CacheKey), "one read (and one signature check) per start");
        }
    }

    /// <summary>One <c>/entitlements</c> request at a time, answers in the server's order, a refresh after an expiry.</summary>
    public sealed class RefreshTests
    {
        private static ApiResult<CustomerInfoDto> Answer(long serverTimeMs, Dictionary<string, long> subs = null, params string[] owned)
        {
            var dto = FakeApi.Info(subs, null, owned);
            dto.ServerTimeMs = serverTimeMs;
            return new ApiResult<CustomerInfoDto>(200, dto, null);
        }

        [Test]
        public void Refresh_WhileOneIsRunning_SharesOneRequestThatFollowsIt()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var slow = new TaskCompletionSource<ApiResult<CustomerInfoDto>>();
                h.Api.HoldEntitlements = slow;

                var first = h.Service.RefreshCustomerInfoAsync();
                var second = h.Service.RefreshCustomerInfoAsync();
                var third = h.Service.RefreshCustomerInfoAsync();
                h.Service.OnAppResumed();

                Assert.AreEqual(1, h.Api.EntitlementsCalls, "one request in flight");
                Assert.IsFalse(first.IsCompleted || second.IsCompleted || third.IsCompleted);

                // What the server knew when the first request was made, then what it knows now.
                h.Api.EntitlementsResult = Answer(h.NowMs + 1, null, "no_ads");
                slow.SetResult(Answer(h.NowMs));

                Assert.AreEqual(2, h.Api.EntitlementsCalls, "everybody who asked meanwhile shares one more request");
                Assert.IsFalse(Sync.Run(first).Owns("no_ads"));
                Assert.IsTrue(Sync.Run(second).Owns("no_ads"), "asked after the first request started: its answer is not older than the call");
                Assert.AreSame(Sync.Run(second), Sync.Run(third));
                Assert.IsTrue(h.Service.IsActive("no_ads"));

                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(3, h.Api.EntitlementsCalls, "nothing in flight: a new request");
            }
        }

        [Test]
        public void Refresh_CancellingStopsTheWait_NotTheRequest()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var slow = new TaskCompletionSource<ApiResult<CustomerInfoDto>>();
                h.Api.HoldEntitlements = slow;
                using (var cts = new CancellationTokenSource())
                {
                    var refresh = h.Service.RefreshCustomerInfoAsync(cts.Token);
                    cts.Cancel();
                    Assert.IsTrue(refresh.IsCanceled);
                }

                slow.SetResult(Answer(h.NowMs, null, "no_ads"));
                Assert.IsTrue(h.Service.IsActive("no_ads"), "the answer still arrives");
                Assert.AreEqual(1, h.Api.EntitlementsCalls);
            }
        }

        [Test]
        public void AnAnswerOlderThanTheCurrentOne_IsDropped()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                h.Api.EntitlementsResult = Answer(h.NowMs, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.IsTrue(h.Service.IsActive("no_ads"));
                var changes = h.Changes.Count;

                // An answer the server made earlier arrives later (a slow connection, a retry).
                h.Api.EntitlementsResult = Answer(h.NowMs - 5000);
                var info = Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.IsTrue(info.Owns("no_ads"), "the caller gets the current customer info");
                Assert.IsTrue(h.Service.IsActive("no_ads"));
                Assert.AreEqual(changes, h.Changes.Count);

                // A /restore answer goes the same way.
                h.Store.Owned.Add(FakeStore.GoogleOrder("gems_100"));
                var stale = FakeApi.Info(null, null);
                stale.ServerTimeMs = h.NowMs - 1;
                h.Api.RestoreResult = new ApiResult<RestoreResponse>(200, new RestoreResponse { CustomerInfo = stale }, null);
                Assert.IsTrue(Sync.Run(h.Service.RestoreAsync()).Owns("no_ads"));

                // A newer one that takes it away is applied.
                h.Api.EntitlementsResult = Answer(h.NowMs + 1);
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.IsFalse(h.Service.IsActive("no_ads"));

                // After a restart the cached copy does not block the first answer, whatever its time.
                h.Service.Dispose();
                h.Api.EntitlementsResult = Answer(h.NowMs - 60_000, null, "no_ads");
                h.Start();
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.IsTrue(h.Service.IsActive("no_ads"));
            }
        }

        [Test]
        public void Tick_AsksTheServerShortlyAfterAnExpiry_AtMostOnceAMinute()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var expiry = h.NowMs + 3_600_000;
                var subs = new Dictionary<string, long> { ["premium"] = expiry };
                h.Api.EntitlementsResult = Answer(h.NowMs, subs);
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                var calls = h.Api.EntitlementsCalls;

                h.Service.OnTick();
                h.Now = h.Now.AddHours(1).AddSeconds(4);
                h.Service.OnTick();
                Assert.AreEqual(calls, h.Api.EntitlementsCalls, "not expired, then expired a moment ago: the server may not know yet");

                // The server still lists it (the renewal has not arrived yet).
                h.Now = h.Now.AddSeconds(2);
                h.Api.EntitlementsResult = Answer(h.NowMs, subs);
                h.Service.OnTick();
                Assert.AreEqual(calls + 1, h.Api.EntitlementsCalls, "asked shortly after the expiry");
                h.Service.OnTick();
                h.Now = h.Now.AddSeconds(59);
                h.Api.EntitlementsResult = Answer(h.NowMs, subs);
                h.Service.OnTick();
                Assert.AreEqual(calls + 1, h.Api.EntitlementsCalls, "at most once a minute");

                // The renewal arrived: nothing is due any more.
                h.Now = h.Now.AddSeconds(2);
                h.Api.EntitlementsResult = Answer(h.NowMs, new Dictionary<string, long> { ["premium"] = expiry + 30L * 86_400_000 });
                h.Service.OnTick();
                Assert.AreEqual(calls + 2, h.Api.EntitlementsCalls);
                Assert.IsTrue(h.Service.IsActive("premium"));
                h.Now = h.Now.AddMinutes(5);
                h.Service.OnTick();
                Assert.AreEqual(calls + 2, h.Api.EntitlementsCalls);
            }
        }

        [Test]
        public void Tick_AsksTheServer_WhenAPausedSubscriptionIsDueToResume()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var answer = Answer(h.NowMs);
                answer.Value.Paused = new Dictionary<string, PausedStateDto>
                {
                    ["premium"] = new PausedStateDto { ProductId = "premium", Store = "google", ResumesAtMs = h.NowMs + 60_000 },
                };
                h.Api.EntitlementsResult = answer;
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.IsFalse(h.Service.IsActive("premium"));
                var calls = h.Api.EntitlementsCalls;

                h.Now = h.Now.AddSeconds(70);
                h.Api.EntitlementsResult = Answer(h.NowMs, new Dictionary<string, long> { ["premium"] = h.NowMs + 86_400_000 });
                h.Service.OnTick();

                Assert.AreEqual(calls + 1, h.Api.EntitlementsCalls);
                Assert.IsTrue(h.Service.IsActive("premium"), "resumed");
                Assert.IsEmpty(h.Service.CustomerInfo.Paused);
            }
        }
    }

    /// <summary>What is written to the storage, and when it reaches the disk.</summary>
    public sealed class StorageTests
    {
        private static readonly string InfoKey = "purser.customer_info." + ServiceHarness.CacheScope + ".player-1";

        private static PaywallResponse Paywall(string variant) =>
            new PaywallResponse { Status = "ok", Placement = "main", PaywallId = "pw_1", VariantId = variant, RemoteConfig = JToken.Parse("{\"title\":\"Hi\"}") };

        [Test]
        public void Storage_IsFlushed_WhenTheAppIsPausedAndOnDispose_NotOnEveryWrite()
        {
            var storage = new CountingStorage();
            using (var h = new ServiceHarness { Storage = storage })
            {
                h.Start();
                h.Answer(null, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.Contains(InfoKey, storage.Saves);
                Assert.AreEqual(0, storage.Flushes, "a cache write waits for the next flush");

                h.Service.OnAppPaused();
                Assert.AreEqual(1, storage.Flushes);

                h.Service.SetRefundDataConsent(true);
                Assert.AreEqual(2, storage.Flushes, "the player's answer is saved at once");

                h.Service.Dispose();
                Assert.AreEqual(3, storage.Flushes);
            }
        }

        [Test]
        public void CustomerInfo_ThatDidNotChange_IsNotWrittenAgainForAnHour()
        {
            var storage = new CountingStorage();
            using (var h = new ServiceHarness { Storage = storage })
            {
                h.Start();
                int Writes() => storage.Saves.Count(k => k == InfoKey);
                h.Answer(null, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(1, Writes());

                h.Now = h.Now.AddMinutes(30);
                h.Answer(null, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(1, Writes(), "the same items: only the fetch time would change");
                Assert.AreEqual(h.Now, h.Service.CustomerInfo.FetchedAtUtc, "in memory it is as fresh as the answer");

                h.Now = h.Now.AddMinutes(31);
                h.Answer(null, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(2, Writes(), "after an hour the cached copy gets its new fetch time");

                h.Answer(null, null);
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(3, Writes(), "a change is written at once");

                // A new launch: the first answer equal to the cached copy is not written either.
                h.Service.Dispose();
                h.Start();
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(3, Writes());
            }
        }

        [Test]
        public void Paywall_ThatDidNotChange_IsNotWrittenAgainForAnHour()
        {
            var storage = new CountingStorage();
            var now = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);
            int Writes() => storage.Saves.Count(k => k.StartsWith("purser.paywall."));

            PaywallCache.Save(storage, "app", "u1", "main", Paywall("a"), now, null);
            PaywallCache.Save(storage, "app", "u1", "main", Paywall("a"), now.AddMinutes(10), null);
            Assert.AreEqual(1, Writes());
            Assert.AreEqual(now, PaywallCache.Load(storage, "app", "u1", "main", null).Value.FetchedAtUtc);

            PaywallCache.Save(storage, "app", "u1", "main", Paywall("b"), now.AddMinutes(11), null);
            Assert.AreEqual(2, Writes(), "another variant");
            PaywallCache.Save(storage, "app", "u1", "main", Paywall("b"), now.AddMinutes(80), null);
            Assert.AreEqual(3, Writes(), "an hour later");

            PaywallCache.Save(storage, "app", "u1", "main", null, now.AddMinutes(81), null);
            Assert.IsNull(PaywallCache.Load(storage, "app", "u1", "main", null), "forgotten");
            var writes = Writes();
            PaywallCache.Save(storage, "app", "u1", "main", null, now.AddMinutes(82), null);
            Assert.AreEqual(writes, Writes(), "nothing to forget");
        }

        [Test]
        public void OnlyTheLastPlayersCache_IsKept()
        {
            var storage = new CountingStorage();
            string id = "player-1";
            using (var h = new ServiceHarness { Storage = storage, Users = new FuncUserIdProvider(() => id) })
            {
                h.Start();
                h.Answer(null, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                h.Api.PaywallResult = new ApiResult<PaywallResponse>(200, Paywall("a"), null);
                Sync.Run(h.Service.GetPaywallAsync("main"));
                var paywallKey = storage.Saves.Single(k => k.StartsWith("purser.paywall."));
                Assert.IsFalse(string.IsNullOrEmpty(storage.Peek(InfoKey)));
                Assert.IsFalse(string.IsNullOrEmpty(storage.Peek(paywallKey)));

                // Another player logs in while the server cannot be reached: nothing of theirs to cache yet.
                var answer = h.Api.EntitlementsResult;
                h.Api.EntitlementsResult = new ApiResult<CustomerInfoDto>(0, null, "offline");
                id = "player-2";
                h.Service.OnTick();
                Assert.IsFalse(string.IsNullOrEmpty(storage.Peek(InfoKey)), "kept until the new player has data to cache");
                h.Api.EntitlementsResult = answer;
                Sync.Run(h.Service.RefreshCustomerInfoAsync());

                Assert.IsTrue(string.IsNullOrEmpty(storage.Peek(InfoKey)), "the previous player's customer info is gone");
                Assert.IsTrue(string.IsNullOrEmpty(storage.Peek(paywallKey)), "and their paywalls");
                Assert.IsFalse(string.IsNullOrEmpty(storage.Peek("purser.customer_info." + ServiceHarness.CacheScope + ".player-2")));

                // Back to the first player: nothing cached, so nothing is shown until the server answers.
                id = "player-1";
                h.Api.EntitlementsResult = new ApiResult<CustomerInfoDto>(0, null, "offline");
                h.Service.OnTick();
                Assert.IsFalse(h.Service.IsActive("no_ads"));
            }
        }
    }

    /// <summary>Web checkout orders: a started purchase is polled, a resumed one is only asked about a few times.</summary>
    public sealed class WebCheckoutCheckTests
    {
        private const string User = "player-1";

        private sealed class ScriptedStripe : IStripeCheckoutApi
        {
            public int Checks;
            /// <summary>The answer of every session check.</summary>
            public Func<ApiResult<VerifyResponse>> Next = () => new ApiResult<VerifyResponse>(200,
                new VerifyResponse { Status = VerifyStatus.PendingOrCanceled }, null);

            public Task<ApiResult<StripeCheckoutResponse>> CreateStripeCheckoutAsync(StripeCheckoutRequest request, CancellationToken ct = default) =>
                Task.FromResult(new ApiResult<StripeCheckoutResponse>(200,
                    new StripeCheckoutResponse { Status = "ok", SessionId = "cs_new", Url = "https://checkout.stripe.com/c/pay/cs_new" }, null));

            public Task<ApiResult<VerifyResponse>> VerifyStripeSessionAsync(StripeSessionRequest request, CancellationToken ct = default)
            {
                Checks++;
                return Task.FromResult(Next());
            }
        }

        private BillingConfig m_Config;
        private ScriptedStripe m_Stripe;
        private StripeStoreAdapter m_Adapter;
        private List<TimeSpan> m_Waits;
        private DateTime m_Now;

        [SetUp]
        public void SetUp()
        {
            m_Config = BillingConfig.Create("https://billing.example.com/route", new BillingProductConfig("gems_100", BillingProductType.Consumable));
            m_Config.WithBuild(BillingStores.Stripe, "com.test.game.web");
            m_Stripe = new ScriptedStripe();
            m_Waits = new List<TimeSpan>();
            m_Now = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);
            m_Adapter = new StripeStoreAdapter(m_Config, new StripeCheckoutOptions
            {
                SuccessUrl = "https://game.example/play",
                CancelUrl = "https://game.example/play",
                PollInterval = TimeSpan.FromSeconds(3),
                PaymentTimeout = TimeSpan.FromMinutes(10),
                OpenUrl = _ => { },
            }, m_Stripe, new MemoryBillingStorage(), new BillingLog(BillingLogLevel.None))
            {
                Delay = (d, ct) =>
                {
                    m_Waits.Add(d);
                    m_Now += d;
                    return Task.CompletedTask;
                },
                UtcNow = () => m_Now,
            };
            m_Adapter.PrepareCheckout(new CheckoutContext { UserId = User });
        }

        [TearDown]
        public void TearDown()
        {
            m_Adapter.Dispose();
            UnityEngine.Object.DestroyImmediate(m_Config);
        }

        private StoreOrder Raised(Action raise)
        {
            StoreOrder order = null;
            void Keep(StoreOrder o) => order = o;
            m_Adapter.OrderPending += Keep;
            raise();
            m_Adapter.OrderPending -= Keep;
            Assert.IsNotNull(order, "the adapter raised the order");
            return order;
        }

        [Test]
        public void StartedCheckout_IsPolledUntilThePaymentTimeout()
        {
            var order = Raised(() => m_Adapter.Purchase("gems_100"));

            var answer = Sync.Run(m_Adapter.CheckOrderAsync(order, null, CancellationToken.None));

            Assert.AreEqual(VerifyStatus.PendingOrCanceled, answer.Value.Status);
            Assert.AreEqual(201, m_Stripe.Checks, "every 3 s for 10 minutes: the player is paying");
            Assert.IsTrue(m_Waits.All(w => w == TimeSpan.FromSeconds(3)));
        }

        [Test]
        public void ResumedOrder_IsAskedAboutAFewTimesOnly()
        {
            var order = Raised(() => m_Adapter.ResumeSession("cs_old", "gems_100", User));

            var answer = Sync.Run(m_Adapter.CheckOrderAsync(order, null, CancellationToken.None));

            Assert.AreEqual(VerifyStatus.PendingOrCanceled, answer.Value.Status, "reported as not paid yet");
            Assert.AreEqual(3, m_Stripe.Checks);
            CollectionAssert.AreEqual(new[] { TimeSpan.FromSeconds(3), TimeSpan.FromSeconds(6) }, m_Waits, "with backoff");
            CollectionAssert.AreEqual(new[] { "cs_old" }, m_Adapter.PendingSessionIds, "still remembered for the next launch");

            // The orders of an earlier launch (ConnectAsync raises them) are resumed ones too.
            m_Stripe.Checks = 0;
            var again = Raised(() => Sync.Run(m_Adapter.ConnectAsync(CancellationToken.None)));
            Sync.Run(m_Adapter.CheckOrderAsync(again, null, CancellationToken.None));
            Assert.AreEqual(3, m_Stripe.Checks);

            // A paid one answers at once.
            m_Stripe.Checks = 0;
            m_Stripe.Next = () => new ApiResult<VerifyResponse>(200, new VerifyResponse { Status = VerifyStatus.Success }, null);
            Assert.AreEqual(VerifyStatus.Success, Sync.Run(m_Adapter.CheckOrderAsync(order, null, CancellationToken.None)).Value.Status);
            Assert.AreEqual(1, m_Stripe.Checks);
        }

        [Test]
        public void FailedRequests_EndTheCheck_ForTheServiceToRetryLater()
        {
            var order = Raised(() => m_Adapter.Purchase("gems_100"));
            m_Stripe.Next = () => new ApiResult<VerifyResponse>(0, null, "no network");

            var answer = Sync.Run(m_Adapter.CheckOrderAsync(order, null, CancellationToken.None));

            Assert.IsTrue(answer.IsTransportError, "the billing service retries a transport error on its own schedule");
            Assert.AreEqual(3, m_Stripe.Checks, "not every 3 s for 10 minutes");

            // A failure between answers does not count against the next ones.
            m_Stripe.Checks = 0;
            var n = 0;
            m_Stripe.Next = () => ++n % 2 == 0
                ? new ApiResult<VerifyResponse>(503, null, "HTTP 503")
                : new ApiResult<VerifyResponse>(200, new VerifyResponse { Status = n >= 9 ? VerifyStatus.Success : VerifyStatus.PendingOrCanceled }, null);
            Assert.AreEqual(VerifyStatus.Success, Sync.Run(m_Adapter.CheckOrderAsync(order, null, CancellationToken.None)).Value.Status);
            Assert.AreEqual(9, m_Stripe.Checks);
        }
    }

    /// <summary>Paused subscriptions and Family Sharing in the customer info.</summary>
    public sealed class PausedAndFamilySharedTests
    {
        private const string ServerJson =
            "{\"user_id\":\"player-1\",\"game_key\":\"game\",\"server_time_ms\":1800014400000," +
            "\"access\":{\"premium\":{\"product_id\":\"premium\",\"store\":\"apple\",\"expires_at_ms\":1900000000000,\"purchased_at_ms\":1790000000000," +
            "\"will_renew\":true,\"in_grace\":false,\"billing_issue\":false,\"trial\":false,\"family_shared\":true}}," +
            "\"subscriptions\":{\"premium\":{\"product_id\":\"premium\",\"store\":\"apple\",\"expires_at_ms\":1900000000000,\"purchased_at_ms\":1790000000000," +
            "\"will_renew\":true,\"in_grace\":false,\"billing_issue\":false,\"trial\":false,\"family_shared\":true}}," +
            "\"non_consumables\":[],\"paused\":{\"premium_year\":{\"product_id\":\"premium_year\",\"store\":\"google\",\"resumes_at_ms\":1810000000000}}}";

        private static readonly DateTime Now = new DateTime(2027, 1, 15, 12, 0, 0, DateTimeKind.Utc);

        [Test]
        public void ServerAnswer_IsParsed()
        {
            var dto = JsonConvert.DeserializeObject<CustomerInfoDto>(ServerJson);
            var info = CustomerInfo.FromDto("player-1", dto, Now, false);

            Assert.IsTrue(info.Subscriptions["premium"].IsFamilyShared);
            Assert.IsTrue(info.Access["premium"].IsFamilyShared);
            var paused = info.Paused.Single();
            Assert.AreEqual("premium_year", paused.Key);
            Assert.AreEqual(("premium_year", "google"), (paused.Value.ProductId, paused.Value.Store));
            Assert.AreEqual(DateTimeOffset.FromUnixTimeMilliseconds(1_810_000_000_000).UtcDateTime, paused.Value.ResumesAt);
            Assert.IsFalse(info.IsActive("premium_year", Now), "paused: no access");
            Assert.IsTrue(info.IsActive("premium", Now));

            // An answer of a server without these fields.
            var old = JsonConvert.DeserializeObject<CustomerInfoDto>(
                "{\"user_id\":\"player-1\",\"server_time_ms\":1800014400000,\"access\":{},\"subscriptions\":{\"premium\":" +
                "{\"product_id\":\"premium\",\"store\":\"google\",\"expires_at_ms\":1900000000000}},\"non_consumables\":[]}");
            var before = CustomerInfo.FromDto("player-1", old, Now, false);
            Assert.IsEmpty(before.Paused);
            Assert.IsFalse(before.Subscriptions["premium"].IsFamilyShared);
            Assert.IsNull(new PausedSubscriptionInfo("p", "google", 0).ResumesAt);
        }

        [Test]
        public void Content_IncludesPausedAndFamilyShared()
        {
            CustomerInfo Parse(string json) => CustomerInfo.FromDto("player-1", JsonConvert.DeserializeObject<CustomerInfoDto>(json), Now, false);
            var info = Parse(ServerJson);

            Assert.IsTrue(info.HasSameContent(Parse(ServerJson)));
            Assert.IsFalse(info.HasSameContent(Parse(ServerJson.Replace("\"family_shared\":true}},\"non_consumables\"", "\"family_shared\":false}},\"non_consumables\""))),
                "no longer shared");
            Assert.IsFalse(info.HasSameContent(Parse(ServerJson.Replace("1810000000000", "1820000000000"))), "resumes later");
            Assert.IsFalse(info.HasSameContent(Parse(ServerJson.Replace(",\"paused\":{\"premium_year\":{\"product_id\":\"premium_year\",\"store\":\"google\",\"resumes_at_ms\":1810000000000}}", ""))),
                "resumed");

            // The cache keeps both.
            var storage = new MemoryBillingStorage();
            CustomerInfoCache.Save(storage, "app", "player-1", info.ToDto(), Now, null);
            var cached = CustomerInfoCache.Load(storage, "app", "player-1", null);
            Assert.IsTrue(cached.HasSameContent(info));
            Assert.IsTrue(cached.Subscriptions["premium"].IsFamilyShared);
            Assert.AreEqual(info.Paused["premium_year"].ResumesAt, cached.Paused["premium_year"].ResumesAt);
            Assert.IsTrue(info.AsCached().HasSameContent(info));
        }

        [Test]
        public void Service_APausedProductGivesNoAccess_AndAChangeIsReported()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var dto = JsonConvert.DeserializeObject<CustomerInfoDto>(ServerJson);
                h.Api.EntitlementsResult = new ApiResult<CustomerInfoDto>(200, dto, null);
                Sync.Run(h.Service.RefreshCustomerInfoAsync());

                Assert.IsFalse(h.Service.IsActive("premium_year"));
                Assert.IsNull(h.Service.GetExpiration("premium_year"));
                Assert.IsTrue(h.Service.HasAccess("premium"), "through the shared subscription");
                Assert.AreEqual(1, h.Service.CustomerInfo.Paused.Count);
                var changes = h.Changes.Count;

                // The same answer again: no event. Resumed: an event.
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(changes, h.Changes.Count);
                dto.Paused = null;
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                Assert.AreEqual(changes + 1, h.Changes.Count);
                Assert.IsEmpty(h.Service.CustomerInfo.Paused);
            }
        }
    }

    /// <summary>Lookups built once, log messages built only when written, and the linker file.</summary>
    public sealed class LookupTests
    {
        [Test]
        public void GetProduct_AndIsActive_FindAProductByAnyOfItsIds()
        {
            using (var h = new ServiceHarness())
            {
                h.Start();
                var premium = h.Service.GetProduct("premium");
                Assert.AreEqual("premium_android", premium.StoreId);
                Assert.AreSame(premium, h.Service.GetProduct("premium_android"), "by its store id");
                Assert.IsNull(h.Service.GetProduct("premium_ios"), "another store's id is not a product of this store");
                Assert.IsNull(h.Service.GetProduct("nope"));
                Assert.IsNull(h.Service.GetProduct(null));

                h.Answer(new Dictionary<string, double> { ["premium"] = 10 }, null, "no_ads");
                Sync.Run(h.Service.RefreshCustomerInfoAsync());
                foreach (var id in new[] { "premium", "premium_android", "premium_ios" })
                {
                    Assert.IsTrue(h.Service.IsActive(id), id);
                    Assert.AreEqual(h.Now.AddDays(10), h.Service.GetExpiration(id), id);
                }
                Assert.IsTrue(h.Service.IsActive("no_ads"));
                Assert.IsFalse(h.Service.IsActive("premium_year"));
                Assert.IsFalse(h.Service.IsActive("nope"));
                Assert.IsFalse(h.Service.IsActive(null));
                Assert.IsNull(h.Service.GetExpiration("no_ads"));
                Assert.IsNull(h.Service.GetExpiration(null));
            }
        }

        [Test]
        public void IsActive_BeforeInitialization_UsesTheConfig()
        {
            using (var h = new ServiceHarness())
            {
                CustomerInfoCache.Save(h.Storage, ServiceHarness.CacheScope, "player-1",
                    FakeApi.Info(new Dictionary<string, long> { ["premium"] = h.NowMs + 86_400_000 }, null), h.Now, null);
                h.Start(initialize: false);
                h.Service.SetUserId("player-1");
                Assert.IsTrue(h.Service.IsActive("premium_ios"), "the cached subscription, asked by a store id, before the store is connected");
            }
        }

        [Test]
        public void Log_SaysWhetherALevelIsWritten()
        {
            var written = new List<string>();
            var log = new BillingLog(BillingLogLevel.Warning, (level, message) => written.Add(message));
            Assert.IsTrue(log.IsEnabled(BillingLogLevel.Error));
            Assert.IsTrue(log.IsEnabled(BillingLogLevel.Warning));
            Assert.IsFalse(log.IsEnabled(BillingLogLevel.Info));
            Assert.IsFalse(log.IsEnabled(BillingLogLevel.Verbose));
            Assert.IsFalse(new BillingLog(BillingLogLevel.None).IsEnabled(BillingLogLevel.Error));
            log.Info("not written");
            log.Warning("written");
            CollectionAssert.AreEqual(new[] { "written" }, written);
        }

        [Test]
        public void LinkXml_PreservesEveryTypeJsonCreatesByReflection()
        {
            var path = Path.GetFullPath(Path.Combine(Path.GetDirectoryName(ThisFile()) ?? "", "..", "..", "Runtime", "link.xml"));
            if (!File.Exists(path))
                Assert.Inconclusive("link.xml of com.yevhenii.purser not found at " + path);
            var xml = File.ReadAllText(path);

            const BindingFlags all = BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly;
            var types = typeof(BillingService).Assembly.GetTypes()
                .Where(t => t.GetMembers(all).Any(m => m.GetCustomAttribute<JsonPropertyAttribute>() != null))
                .Select(t => t.FullName.Replace('+', '/'))
                .ToList();
            Assert.Greater(types.Count, 20);
            var missing = types.Where(t => !xml.Contains("<type fullname=\"" + t + "\" preserve=\"all\"")).ToList();
            Assert.IsEmpty(missing, "types with [JsonProperty] that link.xml does not preserve");

            // What the promotional offer bridge reflects into (UnityIapApplePromotionalOffers).
            StringAssert.Contains("<assembly fullname=\"Unity.Purchasing.Stores\" ignoreIfMissing=\"1\">", xml);
            StringAssert.Contains("UnityEngine.Purchasing.AppleStoreImpl", xml);
            StringAssert.Contains("Purchasing.Utilities.StoreKitSelector", xml);
        }

        private static string ThisFile([CallerFilePath] string path = "") => path;
    }
}
