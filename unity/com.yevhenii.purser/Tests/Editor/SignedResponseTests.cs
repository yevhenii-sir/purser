using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>
    /// Ed25519 verification with RFC 8032 section 7.1 test vectors (TEST 1-3). The public keys and signatures below
    /// were produced by running the RFC 8032 section 6 reference implementation (Python) on the vectors' secret keys
    /// and messages; the invalid cases were derived from TEST 2 and checked to fail with the same implementation.
    /// </summary>
    public sealed class Ed25519Tests
    {
        private const string Test1PublicKey = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
        private const string Test1Signature = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";
        private const string Test2PublicKey = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
        private const string Test2Signature = "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00";
        private const string Test3PublicKey = "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025";
        private const string Test3Signature = "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a";
        /// <summary>TEST 2 with S + L in place of S: the group equation still holds, but S is not canonical.</summary>
        private const string Test2NonCanonicalS = "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69daf52db7415978abc61b2c2eb6aeebfca0387b2eaeb4302aeeb00d291612bb0c10";

        internal static byte[] Hex(string hex)
        {
            var bytes = new byte[hex.Length / 2];
            for (var i = 0; i < bytes.Length; i++)
                bytes[i] = Convert.ToByte(hex.Substring(i * 2, 2), 16);
            return bytes;
        }

        [Test]
        public void Rfc8032Vectors_Verify()
        {
            Assert.IsTrue(Ed25519.Verify(Hex(Test1PublicKey), new byte[0], Hex(Test1Signature)), "TEST 1 (empty message)");
            Assert.IsTrue(Ed25519.Verify(Hex(Test2PublicKey), Hex("72"), Hex(Test2Signature)), "TEST 2");
            Assert.IsTrue(Ed25519.Verify(Hex(Test3PublicKey), Hex("af82"), Hex(Test3Signature)), "TEST 3");
        }

        [Test]
        public void WrongKey_IsRejected()
        {
            Assert.IsFalse(Ed25519.Verify(Hex(Test1PublicKey), Hex("72"), Hex(Test2Signature)));
        }

        [Test]
        public void ChangedMessage_IsRejected()
        {
            Assert.IsFalse(Ed25519.Verify(Hex(Test2PublicKey), Hex("73"), Hex(Test2Signature)));
            Assert.IsFalse(Ed25519.Verify(Hex(Test1PublicKey), Hex("00"), Hex(Test1Signature)));
        }

        [Test]
        public void FlippedSignatureBits_AreRejected()
        {
            foreach (var bit in new[] { 0, 7, 100, 255, 256, 320, 500 })
            {
                var signature = Hex(Test2Signature);
                signature[bit / 8] ^= (byte)(1 << (bit % 8));
                Assert.IsFalse(Ed25519.Verify(Hex(Test2PublicKey), Hex("72"), signature), $"bit {bit}");
            }
        }

        [Test]
        public void NonCanonicalS_IsRejected()
        {
            Assert.IsFalse(Ed25519.Verify(Hex(Test2PublicKey), Hex("72"), Hex(Test2NonCanonicalS)));
        }

        [Test]
        public void MalformedInput_IsRejected()
        {
            // y = p (2^255 - 19) is not a valid encoding of a point.
            var notAPoint = Hex("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
            Assert.IsFalse(Ed25519.Verify(notAPoint, Hex("72"), Hex(Test2Signature)), "public key not on the curve");
            var badR = Hex(Test2Signature);
            Array.Copy(notAPoint, badR, 32);
            Assert.IsFalse(Ed25519.Verify(Hex(Test2PublicKey), Hex("72"), badR), "R not on the curve");
            Assert.IsFalse(Ed25519.Verify(Hex(Test2PublicKey).Take(31).ToArray(), Hex("72"), Hex(Test2Signature)), "short key");
            Assert.IsFalse(Ed25519.Verify(Hex(Test2PublicKey), Hex("72"), Hex(Test2Signature).Take(63).ToArray()), "short signature");
            Assert.IsFalse(Ed25519.Verify(null, Hex("72"), Hex(Test2Signature)));
        }

        [Test]
        public void ResponseSignature_ChecksTheHeader()
        {
            var key = Convert.ToBase64String(Hex(Test2PublicKey));
            var signature = Convert.ToBase64String(Hex(Test2Signature));
            Assert.IsNull(ResponseSignature.Problem(key, Hex("72"), signature));
            StringAssert.Contains("no X-Purser-Signature", ResponseSignature.Problem(key, Hex("72"), null));
            StringAssert.Contains("not base64", ResponseSignature.Problem(key, Hex("72"), "%%%"));
            StringAssert.Contains("invalid", ResponseSignature.Problem(key, Hex("73"), signature));
            StringAssert.Contains("32 bytes", ResponseSignature.Problem("c2hvcnQ=", Hex("72"), signature));
            Assert.IsNull(ResponseSignature.DecodeKey("not a key"));
        }
    }

    /// <summary>Signed <c>/entitlements</c> and <c>/restore</c> answers (<see cref="BillingConfig.responseSigningPublicKey"/>).</summary>
    public sealed class SignedResponseTests
    {
        /// <summary>RFC 8032 TEST 1 public key, base64.</summary>
        private const string PublicKey = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

        /// <summary>A <c>/entitlements</c> answer signed with the RFC 8032 TEST 1 secret key (signature made with the RFC 8032
        /// reference implementation over these exact UTF-8 bytes).</summary>
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

        private static HttpResponse Signed(string body, string signature) =>
            new HttpResponse(200, body, null, new Dictionary<string, string> { ["x-purser-signature"] = signature });

        [Test]
        public void ValidSignature_IsAccepted()
        {
            var transport = new FakeTransport { Next = Signed(EntitlementsBody, EntitlementsSignature) };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport, PublicKey);
            var result = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { AppId = "a", UserId = "u1" }));
            Assert.IsTrue(result.IsHttpSuccess);
            Assert.IsNull(result.Error);
            Assert.AreEqual("premium", result.Value.Access["premium"].ProductId);

            transport.Next = Signed(RestoreBody, RestoreSignature);
            var restore = Sync.Run(api.RestoreAsync(new RestoreRequest { AppId = "a", UserId = "u1" }));
            CollectionAssert.AreEqual(new[] { "vip" }, restore.Value.CustomerInfo.NonConsumables);
        }

        [Test]
        public void MissingOrWrongSignature_IsAnError()
        {
            var transport = new FakeTransport { Next = new HttpResponse(200, EntitlementsBody) };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport, PublicKey);

            var unsigned = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { UserId = "u1" }));
            Assert.IsNull(unsigned.Value);
            StringAssert.Contains("X-Purser-Signature", unsigned.Error);

            transport.Next = Signed(EntitlementsBody.Replace("1900000000000", "1999999999999"), EntitlementsSignature);
            var tampered = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest { UserId = "u1" }));
            Assert.IsNull(tampered.Value, "a changed body fails the signature");

            transport.Next = Signed(RestoreBody, EntitlementsSignature);
            Assert.IsNull(Sync.Run(api.RestoreAsync(new RestoreRequest { UserId = "u1" })).Value?.CustomerInfo, "another body's signature");
        }

        [Test]
        public void WithoutAKey_AnswersAreNotChecked_AndOtherRoutesNever()
        {
            var transport = new FakeTransport { Next = new HttpResponse(200, EntitlementsBody) };
            var plain = new BillingApiClient("https://b.example.com/route", 10, transport);
            Assert.IsNotNull(Sync.Run(plain.EntitlementsAsync(new EntitlementsRequest())).Value);

            transport.Next = new HttpResponse(200, "{\"status\":\"success\"}");
            var signed = new BillingApiClient("https://b.example.com/route", 10, transport, PublicKey);
            Assert.AreEqual(VerifyStatus.Success, Sync.Run(signed.VerifyAsync(BillingStores.Google, new VerifyRequest())).Value.Status);
        }

        [Test]
        public void Service_KeepsTheCachedCustomerInfo_WhenTheAnswerIsNotSigned()
        {
            var config = BillingConfig.Create("https://billing.example.com/route",
                new BillingProductConfig("premium", BillingProductType.Subscription).WithAccess("premium"),
                new BillingProductConfig("vip", BillingProductType.NonConsumable));
            config.refreshEntitlementsOnInit = false;
            config.restoreOnFirstLaunch = false;
            config.logLevel = BillingLogLevel.None;
            config.responseSigningPublicKey = PublicKey;
            var now = DateTimeOffset.FromUnixTimeMilliseconds(1_800_000_000_000).UtcDateTime;
            var transport = new FakeTransport { Next = Signed(EntitlementsBody, EntitlementsSignature) };
            var store = new FakeStore();
            var service = new BillingService(config, new BillingDependencies
            {
                Store = store,
                Api = new BillingApiClient(config, transport),
                UserIdProvider = new StaticUserIdProvider("u1"),
                Storage = new MemoryBillingStorage(),
                UtcNow = () => now,
                Delay = (d, ct) => Task.CompletedTask,
                IsEditor = false,
            });
            try
            {
                Sync.Run(service.InitializeAsync());
                Sync.Run(service.RefreshCustomerInfoAsync());
                Assert.IsTrue(service.HasAccess("premium"));
                Assert.IsTrue(service.IsActive("vip"));
                var changes = 0;
                service.CustomerInfoChanged += _ => changes++;

                // A forged answer that takes everything away: refused, the previous customer info stays.
                transport.Next = new HttpResponse(200, "{\"user_id\":\"u1\",\"server_time_ms\":1800000000000,\"access\":{},\"subscriptions\":{},\"non_consumables\":[]}");
                var info = Sync.Run(service.RefreshCustomerInfoAsync());
                Assert.IsTrue(info.IsFromCache);
                Assert.IsTrue(service.HasAccess("premium"));
                Assert.IsTrue(service.IsActive("vip"));
                Assert.AreEqual(0, changes);

                // Restore: the unsigned answer is refused too, and the fallback refresh as well.
                store.Owned.Add(FakeStore.GoogleOrder("vip"));
                transport.Next = new HttpResponse(200, RestoreBody);
                Assert.IsTrue(Sync.Run(service.RestoreAsync()).HasAccess("premium", now));
                Assert.IsTrue(transport.Requests.Any(r => r.Url.EndsWith("/restore")));
                Assert.AreEqual(0, changes);
            }
            finally
            {
                service.Dispose();
                UnityEngine.Object.DestroyImmediate(config);
            }
        }
    }
}
