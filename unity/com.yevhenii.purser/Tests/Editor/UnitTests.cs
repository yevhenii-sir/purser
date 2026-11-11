using System;
using System.Collections.Generic;
using System.Linq;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;
using NUnit.Framework;
using UnityEngine;

namespace Purser.Tests
{
    public sealed class AccountIdsTests
    {
        // Same vectors as server/core/src/lib.rs (account_id_tests).
        private const string Identity = "c200fdb63a0e9a9b605e424fd437da9249eb198086994a486efa7f115244b8ba";

        [Test]
        public void GoogleAccountId_MatchesServer()
        {
            Assert.AreEqual(Identity, AccountIds.GoogleAccountId(Identity));
            Assert.AreEqual("player-42", AccountIds.GoogleAccountId("player-42"));
            Assert.AreEqual("9537c5fdf120482f7d58d25e9ed583f52c02b4e304ea814db1633ad565aed7e9",
                AccountIds.GoogleAccountId(new string('x', 65)));
        }

        [Test]
        public void AppleAccountToken_MatchesServer()
        {
            Assert.AreEqual("b355ff39-27d8-8191-b44b-07b0fa0ed98e", AccountIds.AppleAccountToken(Identity).ToString());
            Assert.AreEqual("c3214538-d133-89d9-b4db-63a435cddcae", AccountIds.AppleAccountToken("player-42").ToString());
        }
    }

    public sealed class ReceiptParserTests
    {
        private static string GoogleReceipt(string token, string orderId)
        {
            var purchase = JsonConvert.SerializeObject(new
            {
                orderId,
                packageName = "com.test.game",
                productId = "gems_100",
                purchaseTime = 1700000000000L,
                purchaseState = 0,
                purchaseToken = token,
                acknowledged = false,
            });
            var payload = JsonConvert.SerializeObject(new { json = purchase, signature = "sig==", skuDetails = new string[0] });
            return JsonConvert.SerializeObject(new { Store = "GooglePlay", TransactionID = token, Payload = payload });
        }

        [Test]
        public void ExtractsGoogleTokenAndOrderId()
        {
            var receipt = GoogleReceipt("abc.DEF-123", "GPA.1234-5678");
            Assert.IsTrue(ReceiptParser.TryGetGooglePurchaseToken(receipt, out var token));
            Assert.AreEqual("abc.DEF-123", token);
            Assert.IsTrue(ReceiptParser.TryGetGoogleOrderId(receipt, out var orderId));
            Assert.AreEqual("GPA.1234-5678", orderId);
            Assert.IsTrue(ReceiptParser.TryGetTransactionId(receipt, out var tx));
            Assert.AreEqual("abc.DEF-123", tx);
        }

        [Test]
        public void AppleReceiptHasOnlyTransactionId()
        {
            var receipt = JsonConvert.SerializeObject(new { Store = "AppleAppStore", TransactionID = "2000000001", Payload = "MIIT..." });
            Assert.IsFalse(ReceiptParser.TryGetGooglePurchaseToken(receipt, out _));
            Assert.IsTrue(ReceiptParser.TryGetTransactionId(receipt, out var tx));
            Assert.AreEqual("2000000001", tx);
        }

        [Test]
        public void GarbageIsHandled()
        {
            Assert.IsFalse(ReceiptParser.TryGetGooglePurchaseToken(null, out _));
            Assert.IsFalse(ReceiptParser.TryGetGooglePurchaseToken("not json", out _));
            Assert.IsFalse(ReceiptParser.TryGetTransactionId("{}", out _));
        }
    }

    public sealed class BillingApiClientTests
    {
        [Test]
        public void Verify_PostsSnakeCaseJsonToStoreRoute()
        {
            var transport = new FakeTransport { Next = new HttpResponse(200, "{\"status\":\"success\",\"transaction_id\":\"GPA.1\",\"expires_date_ms\":5}") };
            var api = new BillingApiClient("https://billing.example.com/route/", 10, transport);

            var result = Sync.Run(api.VerifyAsync(BillingStores.Google, new VerifyRequest
            {
                BundleId = "com.test.game",
                UserId = "u1",
                ProductId = "gems_100",
                PurchaseToken = "tok",
                PriceMicros = 990000,
            }));

            var (method, url, body) = transport.Requests[0];
            Assert.AreEqual("POST", method);
            Assert.AreEqual("https://billing.example.com/route/verify/google", url);
            var json = JObject.Parse(body);
            Assert.AreEqual("com.test.game", (string)json["bundle_id"]);
            Assert.AreEqual("tok", (string)json["purchase_token"]);
            Assert.AreEqual(990000, (long)json["price_micros"]);
            Assert.IsFalse(json.ContainsKey("order_id"), "null fields are omitted");

            Assert.IsTrue(result.IsHttpSuccess);
            Assert.AreEqual(VerifyStatus.Success, result.Value.Status);
            Assert.AreEqual(5, result.Value.ExpiresDateMs);
        }

        [Test]
        public void ErrorStatusStillParsesBody()
        {
            var transport = new FakeTransport { Next = new HttpResponse(400, "{\"status\":\"invalid\",\"message\":\"Invalid Value\"}") };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);
            var result = Sync.Run(api.VerifyAsync(BillingStores.Apple, new VerifyRequest()));
            Assert.AreEqual("https://b.example.com/route/verify/apple", transport.Requests[0].Url);
            Assert.AreEqual(400, result.HttpStatus);
            Assert.AreEqual(VerifyStatus.Invalid, result.Value.Status);
            Assert.IsNotNull(result.Error);
        }

        [Test]
        public void TransportErrorAndBadJson()
        {
            var transport = new FakeTransport { Next = new HttpResponse(0, null, "timeout") };
            var api = new BillingApiClient("https://b.example.com/route", 10, transport);
            var offline = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest()));
            Assert.IsTrue(offline.IsTransportError);
            Assert.AreEqual("timeout", offline.Error);

            transport.Next = new HttpResponse(502, "<html>bad gateway</html>");
            var html = Sync.Run(api.EntitlementsAsync(new EntitlementsRequest()));
            Assert.IsNull(html.Value);
            StringAssert.Contains("invalid JSON", html.Error);
        }

        [Test]
        public void EntitlementsDtoParsesMaps()
        {
            var dto = JsonConvert.DeserializeObject<EntitlementsDto>(
                "{\"game_key\":\"g\",\"non_consumables\":[\"vip\"],\"server_time_ms\":10,\"subscriptions\":{\"month_sub\":1793876523123},\"user_id\":\"u\"}");
            Assert.AreEqual(1793876523123, dto.Subscriptions["month_sub"]);
            CollectionAssert.AreEqual(new[] { "vip" }, dto.NonConsumables);
        }
    }

    public sealed class ConfigAndEntitlementsTests
    {
        [Test]
        public void Validate_ReportsProblems()
        {
            var config = BillingConfig.Create("http://insecure.example.com/",
                new BillingProductConfig("a", BillingProductType.Consumable),
                new BillingProductConfig("a", BillingProductType.Consumable),
                new BillingProductConfig("", BillingProductType.Consumable));
            var problems = config.Validate();
            Assert.That(problems, Has.Some.Contains("https"));
            Assert.That(problems, Has.Some.Contains("Duplicate"));
            Assert.That(problems, Has.Some.Contains("no id"));
            UnityEngine.Object.DestroyImmediate(config);

            var good = BillingConfig.Create("https://maincloud.spacetimedb.com/v1/database/test/route",
                new BillingProductConfig("a", BillingProductType.Consumable));
            CollectionAssert.IsEmpty(good.Validate());
            UnityEngine.Object.DestroyImmediate(good);

            var placeholder = BillingConfig.Create(BillingConfig.DefaultServerUrl, new BillingProductConfig("a", BillingProductType.Consumable));
            Assert.That(placeholder.Validate(), Has.Some.Contains(BillingConfig.ServerUrlPlaceholder));
            UnityEngine.Object.DestroyImmediate(placeholder);
        }

        [Test]
        public void MergeJson_TakesTheAdminPanelConfig()
        {
            var config = BillingConfig.Create(BillingConfig.DefaultServerUrl, new BillingProductConfig("old", BillingProductType.Consumable));
            config.requestTimeoutSeconds = 7;
            config.MergeJson(@"{""serverBaseUrl"": ""https://maincloud.spacetimedb.com/v1/database/my-billing/route/"",
                ""builds"": [{""store"": ""google"", ""appId"": ""com.example.mygame""}, {""store"": ""apple"", ""appId"": ""com.example.mygame.ios""},
                             {""store"": ""stripe"", ""appId"": ""com.example.mygame.web""}],
                ""products"": [{""id"": ""gems_100"", ""type"": ""Consumable""}, {""id"": ""no_ads"", ""type"": ""non_consumable""},
                               {""id"": ""vip"", ""type"": ""Subscription"", ""storeIds"": {""apple"": ""vip_ios""}, ""price"": 4.99, ""currency"": ""usd"", ""period"": ""p1m""}]}");

            Assert.AreEqual("https://maincloud.spacetimedb.com/v1/database/my-billing/route", config.serverBaseUrl);
            Assert.AreEqual("com.example.mygame.ios", config.AppIdFor(BillingStores.Apple));
            Assert.AreEqual("com.example.mygame.web", config.AppIdFor(BillingStores.Stripe));
            Assert.AreEqual(Application.identifier, config.AppIdFor(BillingStores.Xsolla), "a store without a build");
            Assert.AreEqual(7, config.requestTimeoutSeconds, "fields missing in the JSON are kept");
            Assert.AreEqual(4, config.products.Count, "the product only this config has stays");
            Assert.IsNotNull(config.FindById("old"));
            Assert.AreEqual(BillingProductType.NonConsumable, config.FindById("no_ads").type);
            var vip = config.FindById("vip");
            Assert.AreEqual("vip_ios", vip.StoreId(BillingStores.Apple));
            Assert.AreEqual("vip", vip.StoreId(BillingStores.Google));
            Assert.AreEqual((4.99, "USD"), (vip.fallbackPrice, vip.fallbackCurrency));
            Assert.AreEqual("P1M", vip.fallbackPeriod);
            Assert.IsFalse(config.FindById("gems_100").HasFallbackPrice);
            Assert.IsFalse(config.FindById("gems_100").HasFallbackPeriod);
            CollectionAssert.IsEmpty(config.Validate());
            Assert.Throws<FormatException>(() => config.MergeJson(@"{""products"": [{""id"": ""x"", ""type"": ""gold""}]}"));
            Assert.Throws<FormatException>(() => config.MergeJson(@"{""androidAppId"": ""com.example.mygame""}"), "the old format");
            Assert.Throws<FormatException>(() => config.MergeJson("not json"));
            Assert.Throws<FormatException>(() => config.MergeJson(@"{""products"": [{""id"": ""x"", ""type"": ""Consumable"", ""price"": ""cheap""}]}"));
            Assert.Throws<FormatException>(() => config.MergeJson(@"{""products"": [{""id"": ""x"", ""type"": ""Subscription"", ""period"": ""monthly""}]}"));
            UnityEngine.Object.DestroyImmediate(config);
        }

        [Test]
        public void ToJson_RoundTripsThroughMergeJson()
        {
            var config = BillingConfig.Create("https://b.example.com/route",
                new BillingProductConfig("gems_100", BillingProductType.Consumable),
                new BillingProductConfig("vip", BillingProductType.Subscription).WithStoreId(BillingStores.Apple, "com.example.vip")
                    .WithFallbackPrice(4.99, "EUR").WithFallbackPeriod("P1Y"));
            config.WithBuild(BillingStores.Google, "com.example.mygame").WithBuild(BillingStores.Xsolla, "com.example.mygame.web");

            var json = JObject.Parse(config.ToJson());
            Assert.AreEqual("xsolla", (string)json["builds"][1]["store"]);
            Assert.IsNull(json["products"][0]["storeIds"], "no overrides, no storeIds");
            Assert.AreEqual("com.example.vip", (string)json["products"][1]["storeIds"]["apple"]);
            Assert.AreEqual((4.99, "EUR"), ((double)json["products"][1]["price"], (string)json["products"][1]["currency"]));
            Assert.IsNull(json["products"][0]["price"], "no fallback price, no price");
            Assert.AreEqual("P1Y", (string)json["products"][1]["period"]);
            Assert.IsNull(json["products"][0]["period"]);

            var copy = BillingConfig.Create("https://other.example.com/route");
            copy.MergeJson(config.ToJson());
            Assert.AreEqual(config.serverBaseUrl, copy.serverBaseUrl);
            Assert.AreEqual("com.example.mygame.web", copy.AppIdFor(BillingStores.Xsolla));
            Assert.AreEqual("com.example.vip", copy.products[1].StoreId(BillingStores.Apple));
            Assert.AreEqual(BillingProductType.Subscription, copy.products[1].type);
            Assert.AreEqual((4.99, "EUR"), (copy.products[1].fallbackPrice, copy.products[1].fallbackCurrency));
            Assert.AreEqual("P1Y", copy.products[1].fallbackPeriod);
            UnityEngine.Object.DestroyImmediate(config);
            UnityEngine.Object.DestroyImmediate(copy);
        }

        private const string AdminConfig = @"{
            ""builds"": [{""store"": ""google"", ""appId"": ""com.example.mygame""}, {""store"": ""apple"", ""appId"": ""com.example.mygame.ios""}],
            ""products"": [{""id"": ""com.example.premium"", ""type"": ""Subscription"", ""price"": 12.99, ""currency"": ""USD""},
                           {""id"": ""gems_100"", ""type"": ""Consumable""}]}";

        private static BillingConfig UnityFirstConfig()
        {
            var config = BillingConfig.Create("https://b.example.com/route",
                new BillingProductConfig("premium", BillingProductType.Subscription)
                    .WithStoreId(BillingStores.Apple, "com.example.premium").WithFallbackPrice(9.99, "USD"),
                new BillingProductConfig("unity_only", BillingProductType.Consumable));
            config.WithBuild(BillingStores.Google, "com.example.mygame").WithBuild(BillingStores.Steam, "480");
            return config;
        }

        [Test]
        public void MergeJson_Additive_AddsAndUpdatesAndKeepsTheRest()
        {
            var config = UnityFirstConfig();

            var preview = config.PreviewJson(AdminConfig).Select(c => c.ToString()).ToList();
            Assert.AreEqual(2, config.products.Count, "a preview changes nothing");
            Assert.AreEqual(2, config.builds.Count);
            CollectionAssert.AreEqual(new[]
            {
                "+ Build apple: com.example.mygame.ios",
                "~ Product premium: price 9.99 USD → 12.99 USD",
                "+ Product gems_100: Consumable",
            }, preview);

            CollectionAssert.AreEqual(preview, config.MergeJson(AdminConfig).Select(c => c.ToString()));
            Assert.AreEqual(12.99, config.FindById("premium").fallbackPrice, "matched by its App Store id");
            Assert.AreEqual("com.example.premium", config.FindById("premium").StoreId(BillingStores.Apple));
            Assert.IsNotNull(config.FindById("unity_only"));
            Assert.AreEqual("480", config.AppIdFor(BillingStores.Steam));
            CollectionAssert.IsEmpty(config.PreviewJson(AdminConfig), "nothing left to change");
            UnityEngine.Object.DestroyImmediate(config);
        }

        [Test]
        public void MergeJson_Mirror_RemovesWhatTheAdminPanelDoesNotHave()
        {
            var config = UnityFirstConfig();

            var changes = config.MergeJson(AdminConfig, ConfigMergeMode.Mirror).Select(c => c.ToString()).ToList();

            Assert.That(changes, Has.Member("- Build steam: 480"));
            Assert.That(changes, Has.Member("- Product unity_only: Consumable"));
            Assert.IsNull(config.FindById("unity_only"));
            Assert.AreEqual(Application.identifier, config.AppIdFor(BillingStores.Steam));
            Assert.AreEqual("com.example.premium", config.FindById("premium").StoreId(BillingStores.Apple),
                "the store id the product was matched by is not removed");
            CollectionAssert.AreEquivalent(new[] { "premium", "gems_100" }, config.products.Select(p => p.id));

            // A JSON without products (builds only) does not empty the products.
            config.MergeJson(@"{""builds"": [{""store"": ""google"", ""appId"": ""com.example.mygame""}]}", ConfigMergeMode.Mirror);
            Assert.AreEqual(2, config.products.Count);
            Assert.AreEqual(1, config.builds.Count);
            UnityEngine.Object.DestroyImmediate(config);
        }

        [Test]
        public void MergeJson_InvalidJson_ChangesNothing()
        {
            var config = UnityFirstConfig();
            Assert.Throws<FormatException>(() => config.MergeJson(@"{""builds"": [{""store"": ""apple"", ""appId"": ""com.x""}],
                ""products"": [{""id"": ""x"", ""type"": ""gold""}]}", ConfigMergeMode.Mirror));
            Assert.AreEqual(2, config.builds.Count);
            Assert.AreEqual(2, config.products.Count);
            UnityEngine.Object.DestroyImmediate(config);
        }

        [Test]
        public void Validate_ChecksBuilds()
        {
            var config = BillingConfig.Create("https://b.example.com/route", new BillingProductConfig("a", BillingProductType.Consumable));
            config.builds.Add(new BillingBuild("google", "com.example.mygame"));
            config.builds.Add(new BillingBuild("google", "com.example.other"));
            config.builds.Add(new BillingBuild("apple", ""));
            config.builds.Add(new BillingBuild("paddle", "com.example.mygame.web"));
            config.products[0].fallbackPrice = 0.99;
            var problems = config.Validate();
            Assert.That(problems, Has.Some.Contains("Two builds for the store 'google'"));
            Assert.That(problems, Has.Some.Contains("apple build has no app id"));
            Assert.That(problems, Has.Some.Contains("Unknown store 'paddle'"));
            Assert.That(problems, Has.Some.Contains("three-letter currency"));
            UnityEngine.Object.DestroyImmediate(config);
        }

        [Test]
        public void StoreIdsPerStore()
        {
            var p = new BillingProductConfig("premium", BillingProductType.Subscription).WithStoreId(BillingStores.Google, "premium_android");
            Assert.AreEqual("premium_android", p.StoreId(BillingStores.Google));
            Assert.AreEqual("premium", p.StoreId(BillingStores.Apple));
            Assert.IsTrue(p.Matches("premium_android"));
            CollectionAssert.AreEqual(new[] { "premium", "premium_android" }, p.AllIds());
        }

        [Test]
        public void SubscriptionActivityUsesServerClock()
        {
            var fetched = new DateTime(2027, 1, 1, 0, 0, 0, DateTimeKind.Utc);
            var serverNow = new DateTimeOffset(fetched).ToUnixTimeMilliseconds();
            var e = new Entitlements("u", new Dictionary<string, long>
            {
                ["month"] = serverNow + 60_000,
                ["forever"] = 0,
            }, new[] { "vip" }, serverNow, fetched, false);

            Assert.IsTrue(e.IsActive("month", fetched.AddSeconds(30)));
            Assert.IsFalse(e.IsActive("month", fetched.AddMinutes(2)));
            Assert.IsTrue(e.IsActive("forever", fetched.AddYears(5)));
            Assert.IsTrue(e.IsActive("vip", fetched));
            Assert.IsNull(e.GetExpiration("forever"));
            Assert.AreEqual(fetched.AddMinutes(1), e.GetExpiration("month"));
        }
    }

    public sealed class AppleAdsAttributionTests
    {
        private const string Record = "{\"attribution\":true,\"orgId\":40669820,\"campaignId\":542370539,\"conversionType\":\"Download\"," +
                                      "\"adGroupId\":542317095,\"countryOrRegion\":\"US\",\"keywordId\":87675432,\"adId\":542317136}";

        [Test]
        public void Parse_MapsTheRecordToAttributionKeys()
        {
            var v = AppleAdsAttributionProvider.Parse(Record);
            Assert.AreEqual("542370539", v[AttributionKeys.AppleAdsCampaignId]);
            Assert.AreEqual("87675432", v[AttributionKeys.AppleAdsKeywordId]);
            Assert.AreEqual("US", v[AttributionKeys.AppleAdsCountry]);
            Assert.IsEmpty(AppleAdsAttributionProvider.Parse("{\"attribution\":false}"), "organic install");
        }

        [Test]
        public void AsksAppleOnceAndKeepsTheGamesOwnIds()
        {
            var storage = new MemoryBillingStorage();
            var statuses = new Queue<long>(new long[] { 404, 200 });
            var posts = 0;
            var provider = new AppleAdsAttributionProvider(
                new FuncAttributionProvider(() => new Dictionary<string, string> { [AttributionKeys.AppsFlyerId] = "af-1" }),
                storage, () => "token",
                _ =>
                {
                    posts++;
                    var status = statuses.Dequeue();
                    return System.Threading.Tasks.Task.FromResult((status, status == 200 ? Record : ""));
                },
                _ => System.Threading.Tasks.Task.CompletedTask);

            var first = provider.GetAttributionAsync().GetAwaiter().GetResult();
            Assert.AreEqual("af-1", first[AttributionKeys.AppsFlyerId]);
            Assert.AreEqual("542370539", first[AttributionKeys.AppleAdsCampaignId]);
            Assert.AreEqual(2, posts, "retried while Apple had no record yet");

            var again = new AppleAdsAttributionProvider(null, storage, () => "token",
                _ => throw new AssertionException("the stored answer is reused"));
            Assert.AreEqual("542370539", again.GetAttributionAsync().GetAwaiter().GetResult()[AttributionKeys.AppleAdsCampaignId]);
        }
    }
}
