using System;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using Newtonsoft.Json;
using UnityEngine.Purchasing;

namespace Purser
{
    /// <summary>
    /// Bridge to the promotional offer support inside Unity IAP 5 (checked against the sources of 5.0.1, 5.3.1 and 5.4.4).
    ///
    /// The native StoreKit 2 layer of Unity IAP turns the purchase options JSON into
    /// <c>Product.PurchaseOption.promotionalOffer(offerID:keyID:nonce:signature:timestamp:)</c> when it holds the
    /// top-level keys <c>offerID</c>, <c>keyID</c>, <c>nonce</c>, <c>signature</c> (base64) and <c>timestamp</c>
    /// (<c>PurchaseProductOptionsConverter.swift</c>), but the C# API has no way to pass them:
    /// <c>AppleStoreImpl.PurchaseOptions()</c> only sends <c>appAccountToken</c> and the Ask to Buy flag. This class
    /// calls the internal <c>Purchase(ProductDefinition, string optionsJson)</c> of the App Store implementation by
    /// reflection with complete options. When the internals are not found (another Unity IAP version, StoreKit 1)
    /// it fails instead of buying without the discount.
    /// </summary>
    internal static class UnityIapApplePromotionalOffers
    {
        private const BindingFlags Instance = BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic;
        private const BindingFlags Static = BindingFlags.Static | BindingFlags.Public | BindingFlags.NonPublic;

        /// <summary>Purchase options JSON for the StoreKit 2 bridge of Unity IAP.</summary>
        internal static string BuildOptionsJson(ApplePromotionalOffer offer, bool simulateAskToBuy = false)
        {
            if (offer == null)
                throw new ArgumentNullException(nameof(offer));
            if (!offer.IsWellFormed(out var problem))
                throw new BillingException("Invalid promotional offer: " + problem);
            var options = new Dictionary<string, object>
            {
                { "simulatesAskToBuyInSandbox", simulateAskToBuy },
                { "appAccountToken", offer.AppAccountToken.ToLowerInvariant() },
                { "offerID", offer.OfferId },
                { "keyID", offer.KeyId },
                { "nonce", offer.Nonce.ToLowerInvariant() },
                { "signature", offer.Signature },
                { "timestamp", offer.Timestamp },
            };
            return JsonConvert.SerializeObject(options);
        }

        /// <summary>Starts the purchase; returns false with a reason when Unity IAP cannot apply the offer.</summary>
        internal static bool TryPurchase(ProductDefinition definition, ApplePromotionalOffer offer, out string error)
        {
            error = null;
            try
            {
                var options = BuildOptionsJson(offer, false);
                if (UsesStoreKit1())
                {
                    error = "promotional offers need StoreKit 2 (iOS 15+) with Unity IAP";
                    return false;
                }

                var missing = MissingInternals();
                if (missing != null)
                {
                    error = $"this Unity IAP version does not expose the App Store purchase options ({missing}); " +
                            "promotional offers are supported with com.unity.purchasing 5.0-5.4";
                    return false;
                }
                var storeType = FindType(StoreTypeName);
                var store = storeType.GetField("s_Instance", Static).GetValue(null);
                var purchase = PurchaseMethod(storeType);
                if (store == null)
                {
                    error = "the App Store is not connected";
                    return false;
                }

                options = BuildOptionsJson(offer, ReadAskToBuy(store));
                MarkInFlight(store, definition.storeSpecificId);
                purchase.Invoke(store, new object[] { definition, options });
                return true;
            }
            catch (TargetInvocationException e)
            {
                error = "promotional offer purchase failed: " + (e.InnerException ?? e).Message;
                return false;
            }
            catch (Exception e)
            {
                error = "promotional offer purchase failed: " + e.Message;
                return false;
            }
        }

        private const string StoreTypeName = "UnityEngine.Purchasing.AppleStoreImpl";

        /// <summary>
        /// Null when the Unity IAP internals this bridge needs exist, otherwise what is missing.
        /// Checked by an EditMode test, so a Unity IAP upgrade that breaks the bridge fails the tests.
        /// </summary>
        internal static string MissingInternals()
        {
            var storeType = FindType(StoreTypeName);
            if (storeType == null)
                return StoreTypeName;
            if (storeType.GetField("s_Instance", Static) == null)
                return "AppleStoreImpl.s_Instance";
            if (PurchaseMethod(storeType) == null)
                return "Purchase(ProductDefinition, string)";
            if (FindType("Purchasing.Utilities.StoreKitSelector")?.GetMethod("UseStoreKit1", Static, null, Type.EmptyTypes, null) == null)
                return "StoreKitSelector.UseStoreKit1()";
            return null;
        }

        private static MethodInfo PurchaseMethod(Type storeType) =>
            storeType.GetMethod("Purchase", Instance, null, new[] { typeof(ProductDefinition), typeof(string) }, null);

        private static bool UsesStoreKit1()
        {
            var selector = FindType("Purchasing.Utilities.StoreKitSelector");
            var method = selector?.GetMethod("UseStoreKit1", Static, null, Type.EmptyTypes, null);
            return method != null && (bool)method.Invoke(null, null);
        }

        private static bool ReadAskToBuy(object store)
        {
            var property = store.GetType().GetProperty("simulateAskToBuy", Instance);
            return property?.GetValue(store) is bool value && value;
        }

        /// <summary>Same bookkeeping as <c>AppleStoreImpl.Purchase(ICart)</c> so a duplicate delivery is recognized.</summary>
        private static void MarkInFlight(object store, string storeProductId)
        {
            if (!(FindField(store.GetType(), "m_InFlightPurchases")?.GetValue(store) is Dictionary<string, int> inFlight))
                return;
            inFlight.TryGetValue(storeProductId, out var count);
            inFlight[storeProductId] = count + 1;
        }

        private static FieldInfo FindField(Type type, string name)
        {
            for (var t = type; t != null; t = t.BaseType)
            {
                var field = t.GetField(name, Instance | BindingFlags.DeclaredOnly);
                if (field != null)
                    return field;
            }
            return null;
        }

        private static Type FindType(string fullName) =>
            AppDomain.CurrentDomain.GetAssemblies()
                .Where(a => a.FullName.StartsWith("Unity.Purchasing", StringComparison.Ordinal))
                .Select(a => a.GetType(fullName, false))
                .FirstOrDefault(t => t != null);
    }
}
