using Newtonsoft.Json.Linq;
using NUnit.Framework;

namespace Purser.Tests
{
    /// <summary>The Unity IAP 5 adapter (assembly <c>Purser.UnityIap</c>): default store registration and the App Store
    /// promotional offer bridge.</summary>
    public sealed class UnityIapTests
    {
        private const string User = "player-42";

        private static ApplePromotionalOffer SignedOffer() =>
            new ApplePromotionalOffer("premium_ios", "winback_50", "KEY123ABCD", "a1b2c3d4-0000-4000-8000-00000000000f",
                1_800_000_000_000, "MEUCIQD0ZXN0", AccountIds.AppleAccountToken(User).ToString(), "eyJhbGciOiJFUzI1NiJ9.e30.sig");

        [Test]
        public void UnityIap_IsTheDefaultStore()
        {
            Assert.IsNotNull(Billing.DefaultStoreFactory, "Purser.UnityIap registers its adapter on load");
        }

        [Test]
        public void UnityIapOptions_HoldTheOfferAtTheTopLevel()
        {
            var json = JObject.Parse(UnityIapApplePromotionalOffers.BuildOptionsJson(SignedOffer()));

            Assert.AreEqual("winback_50", (string)json["offerID"]);
            Assert.AreEqual("KEY123ABCD", (string)json["keyID"]);
            Assert.AreEqual("a1b2c3d4-0000-4000-8000-00000000000f", (string)json["nonce"]);
            Assert.AreEqual("MEUCIQD0ZXN0", (string)json["signature"]);
            Assert.AreEqual(JTokenType.Integer, json["timestamp"].Type, "StoreKit bridge reads an Int");
            Assert.AreEqual(1_800_000_000_000, (long)json["timestamp"]);
            Assert.AreEqual(AccountIds.AppleAccountToken(User).ToString(), (string)json["appAccountToken"]);
            Assert.AreEqual(false, (bool)json["simulatesAskToBuyInSandbox"]);
        }

        [Test]
        public void UnityIap_HasTheInternalsTheOfferBridgeUses()
        {
            Assert.IsNull(UnityIapApplePromotionalOffers.MissingInternals());
        }

        [Test]
        public void UnityIapOptions_RejectAMalformedOffer()
        {
            var bad = new ApplePromotionalOffer("premium_ios", "winback_50", "KEY123ABCD", "not-a-uuid", 1, "MEUCIQD0ZXN0",
                AccountIds.AppleAccountToken(User).ToString());
            Assert.IsFalse(bad.IsWellFormed(out _));
            Assert.Throws<BillingException>(() => UnityIapApplePromotionalOffers.BuildOptionsJson(bad));
        }
    }
}
