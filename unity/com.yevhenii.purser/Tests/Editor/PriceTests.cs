using NUnit.Framework;

namespace Purser.Tests
{
    public sealed class PriceTests
    {
        private static StoreProduct Subscription(string localizedPrice, decimal price, string currency, string period) =>
            new StoreProduct("premium", "premium_android", BillingProductType.Subscription, localizedPrice, price, currency,
                "Premium", "", true, subscriptionPeriod: period);

        [Test]
        public void TryParse_SplitsCountAndUnit()
        {
            Assert.IsTrue(BillingPeriods.TryParse("p3m", out var count, out var unit));
            Assert.AreEqual((3, BillingPeriodUnit.Month), (count, unit));
            Assert.IsTrue(BillingPeriods.TryParse("P1Y", out count, out unit));
            Assert.AreEqual((1, BillingPeriodUnit.Year), (count, unit));
            Assert.IsTrue(BillingPeriods.TryParse("P2W", out count, out unit));
            Assert.AreEqual((2, BillingPeriodUnit.Week), (count, unit));
            foreach (var bad in new[] { null, "", "P", "P0M", "1M", "PXM", "P1H", "P-1D" })
                Assert.IsFalse(BillingPeriods.TryParse(bad, out _, out _), bad ?? "null");
        }

        [Test]
        public void PricePer_ConvertsBetweenPeriods()
        {
            Assert.AreEqual(4.1658m, decimal.Round(BillingPeriods.PricePer(49.99m, "P1Y", BillingPeriodUnit.Month).Value, 4));
            Assert.AreEqual(0.9587m, decimal.Round(BillingPeriods.PricePer(49.99m, "P1Y", BillingPeriodUnit.Week).Value, 4));
            Assert.AreEqual(2.99m, BillingPeriods.PricePer(2.99m, "P1W", BillingPeriodUnit.Week), "the same unit is exact");
            Assert.AreEqual(12.99m, decimal.Round(BillingPeriods.PricePer(2.99m, "P1W", BillingPeriodUnit.Month).Value, 2));
            Assert.AreEqual(3.33m, BillingPeriods.PricePer(9.99m, "P3M", BillingPeriodUnit.Month));
            Assert.AreEqual(119.88m, BillingPeriods.PricePer(9.99m, "P1M", BillingPeriodUnit.Year), "months and years are exact");
            Assert.AreEqual(1m, BillingPeriods.PricePer(7m, "P1W", BillingPeriodUnit.Day), "weeks and days are exact");
            Assert.IsNull(BillingPeriods.PricePer(9.99m, "", BillingPeriodUnit.Month));
        }

        [Test]
        public void LocalizedPricePer_FormatsLikeTheStore()
        {
            var yearly = Subscription("$49.99", 49.99m, "USD", "P1Y");
            Assert.AreEqual("$4.17", yearly.LocalizedPricePer(BillingPeriodUnit.Month));
            Assert.AreEqual("$0.96", yearly.LocalizedPricePer(BillingPeriodUnit.Week));
            Assert.AreEqual("$49.99", yearly.LocalizedPricePer(BillingPeriodUnit.Year));
            Assert.IsNull(Subscription("$49.99", 49.99m, "USD", null).LocalizedPricePer(BillingPeriodUnit.Month));
            Assert.AreEqual("4,17 €", Subscription("49,99 €", 49.99m, "EUR", "P1Y").LocalizedPricePer(BillingPeriodUnit.Month));
        }

        [Test]
        public void PricingPhase_PricePer()
        {
            var intro = new PricingPhase(0.99m, "$0.99", "USD", "P1W", 4);
            Assert.AreEqual("$4.30", intro.LocalizedPricePer(BillingPeriodUnit.Month));
            var free = new PricingPhase(0m, "Free", null, "P1W", 1);
            Assert.AreEqual(0m, free.PricePer(BillingPeriodUnit.Month));
        }

        [TestCase("$49.99", 49.99, 4.1658, "USD", "$4.17")]
        [TestCase("49,99 €", 49.99, 4.1658, "EUR", "4,17 €")]
        [TestCase("1.234,56 €", 1234.56, 1234.5, "EUR", "1.234,50 €")]
        [TestCase("$1,299.99", 1299.99, 108.3325, "USD", "$108.33")]
        [TestCase("$1,299.99", 1299.99, 12345.6, "USD", "$12,345.60")]
        [TestCase("¥1,200", 1200, 100, "JPY", "¥100")]
        [TestCase("¥1,200", 1200, 23.077, "JPY", "¥23")]
        [TestCase("¥1,200", 1200, 1500, "JPY", "¥1,500")]
        [TestCase("1 499 ₽", 1499, 124.92, "RUB", "125 ₽")]
        [TestCase("1 499,00 ₽", 1499, 1499, "RUB", "1 499,00 ₽")]
        [TestCase("₹1,099.00", 1099, 91.583, "INR", "₹91.58")]
        [TestCase("CHF 1'234.50", 1234.5, 1234.5, "CHF", "CHF 1'234.50")]
        [TestCase("KWD 1.500", 1.5, 0.125, "KWD", "KWD 0.125")]
        [TestCase("R$ 24,90", 24.9, 2.075, "BRL", "R$ 2,08")]
        [TestCase("$5", 5, 0.42, "USD", "$0.42")]
        [TestCase("US$4.99/mo", 4.99, 1.15, "USD", "US$1.15/mo")]
        [TestCase("", 0, 4.1666, "USD", "4.17 USD")]
        [TestCase("Free", 0, 0, null, "0.00")]
        public void FormatPrice_KeepsTheStoresFormat(string pattern, double patternAmount, double amount, string currency,
            string expected)
        {
            Assert.AreEqual(expected, PriceFormat.Format(pattern, (decimal)patternAmount, (decimal)amount, currency));
        }

        [Test]
        public void FormatPrice_OnAProduct()
        {
            var product = Subscription("1.234,56 €", 1234.56m, "EUR", "P1Y");
            Assert.AreEqual("617,28 €", product.FormatPrice(617.28m));
        }
    }
}
