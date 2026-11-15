using System.Collections.Generic;
using System.Globalization;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

namespace Purser
{
    /// <summary>
    /// Subscription offers from what Unity IAP 5 exposes, without Unity IAP types (tested in the Editor):
    /// Google Play's <c>GoogleProductMetadata</c> (the first offer of the product: free trial, intro price, base phase)
    /// and the App Store's <c>IAppleStoreExtendedProductService.GetProductDetails()</c> JSON (period, introductory
    /// offer, <c>isEligibleForIntroOffer</c>).
    /// </summary>
    internal static class StoreOfferParser
    {
        /// <summary>The offer Unity IAP buys on Google Play, from <c>GoogleProductMetadata</c>.</summary>
        /// <param name="originalJson"><c>GoogleProductMetadata.originalJson</c>: Unity IAP writes the intro price in
        /// micros there (<c>introductoryPriceAmountMicros</c>).</param>
        public static (IReadOnlyList<StoreOffer> Offers, bool? IntroEligible) Google(string subscriptionPeriod, decimal price,
            string localizedPrice, string currency, string freeTrialPeriod, string introductoryPrice, string introductoryPricePeriod,
            int introductoryPriceCycles, string originalJson)
        {
            if (BillingPeriods.Normalize(subscriptionPeriod).Length == 0)
                return (new StoreOffer[0], null);
            var phases = new List<PricingPhase>();
            if (BillingPeriods.Normalize(freeTrialPeriod).Length > 0)
                phases.Add(new PricingPhase(0m, "", currency, freeTrialPeriod, 1));
            var introMicros = Field(originalJson, "introductoryPriceAmountMicros");
            if (BillingPeriods.Normalize(introductoryPricePeriod).Length > 0 &&
                decimal.TryParse(introMicros, NumberStyles.Float, CultureInfo.InvariantCulture, out var micros) && micros > 0)
                phases.Add(new PricingPhase(micros / 1_000_000m, introductoryPrice, currency, introductoryPricePeriod,
                    introductoryPriceCycles <= 0 ? 1 : introductoryPriceCycles));
            var hasIntro = phases.Count > 0;
            phases.Add(new PricingPhase(price, localizedPrice, currency, subscriptionPeriod, 0));
            // Google Play lists only offers the player is eligible for.
            return (new[] { new StoreOffer(null, null, phases) }, hasIntro ? true : (bool?)null);
        }

        /// <summary>Period, introductory offer and eligibility from one product of <c>GetProductDetails()</c>.</summary>
        public static (string Period, IReadOnlyList<StoreOffer> Offers, bool? IntroEligible) Apple(string detailsJson, decimal price,
            string localizedPrice, string currency)
        {
            var none = ("", (IReadOnlyList<StoreOffer>)new StoreOffer[0], (bool?)null);
            JObject details;
            try
            {
                details = string.IsNullOrEmpty(detailsJson) ? null : JObject.Parse(detailsJson);
            }
            catch (JsonException)
            {
                return none;
            }
            if (details == null)
                return none;

            var period = Period(Str(details, "subscriptionNumberOfUnits"), Str(details, "subscriptionPeriodUnit"));
            if (period.Length == 0)
                return none;
            bool? eligible = null;
            if (bool.TryParse(Str(details, "isEligibleForIntroOffer"), out var e))
                eligible = e;

            var offers = new List<StoreOffer>();
            var units = Int(Str(details, "numberOfUnits"));
            var unit = Str(details, "unit");
            var count = Int(Str(details, "introductoryPriceNumberOfPeriods"));
            if (units > 0 && Period(units.ToString(CultureInfo.InvariantCulture), unit).Length > 0 &&
                decimal.TryParse(Str(details, "introductoryPrice"), NumberStyles.Float, CultureInfo.InvariantCulture, out var introPrice))
            {
                count = count <= 0 ? 1 : count;
                PricingPhase intro;
                switch (Str(details, "introductoryType"))
                {
                    case "PayUpFront":
                        intro = new PricingPhase(introPrice, "", currency, Period((units * count).ToString(CultureInfo.InvariantCulture), unit), 1);
                        break;
                    case "FreeTrial":
                        intro = new PricingPhase(0m, "", currency, Period(units.ToString(CultureInfo.InvariantCulture), unit), count);
                        break;
                    default: // PayAsYouGo (StoreKit 1 does not say: a price per period).
                        intro = new PricingPhase(introPrice, "", currency, Period(units.ToString(CultureInfo.InvariantCulture), unit), count);
                        break;
                }
                offers.Add(new StoreOffer(Str(details, "introductoryOfferId"), null, new[]
                {
                    intro,
                    new PricingPhase(price, localizedPrice, currency, period, 0),
                }));
            }
            return (period, offers, eligible);
        }

        /// <summary>ISO 8601 period from StoreKit's number of units and unit (0 day, 1 week, 2 month, 3 year).</summary>
        internal static string Period(string numberOfUnits, string unit)
        {
            var n = Int(numberOfUnits);
            if (n <= 0)
                return "";
            switch ((unit ?? "").Trim())
            {
                case "0": return BillingPeriods.Normalize($"P{n}D");
                case "1": return BillingPeriods.Normalize($"P{n}W");
                case "2": return BillingPeriods.Normalize($"P{n}M");
                case "3": return BillingPeriods.Normalize($"P{n}Y");
                default: return "";
            }
        }

        private static string Field(string json, string name)
        {
            if (string.IsNullOrEmpty(json))
                return null;
            try
            {
                return Str(JObject.Parse(json), name);
            }
            catch (JsonException)
            {
                return null;
            }
        }

        private static string Str(JObject o, string name)
        {
            var token = o[name];
            if (token == null || token.Type == JTokenType.Null)
                return null;
            return token.Type == JTokenType.Float
                ? ((decimal)token).ToString(CultureInfo.InvariantCulture)
                : (string)token;
        }

        private static int Int(string s) =>
            int.TryParse((s ?? "").Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var n) ? n : 0;
    }
}
