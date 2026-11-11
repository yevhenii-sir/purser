using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using UnityEngine;

namespace Purser
{
    public enum BillingProductType
    {
        /// <summary>Can be bought many times (currency packs). The server consumes it after granting.</summary>
        Consumable,
        /// <summary>Bought once, owned forever.</summary>
        NonConsumable,
        /// <summary>Auto-renewable subscription.</summary>
        Subscription,
    }

    /// <summary>The id of a product in one store, when it differs from <see cref="BillingProductConfig.id"/>.</summary>
    [Serializable]
    public sealed class StoreProductId
    {
        [Tooltip("Store id of the billing server: google, apple, amazon, steam, stripe, xsolla (see BillingStores).")]
        public string store;

        [Tooltip("Product id in that store (Amazon: the term SKU of a subscription; Stripe: the Price id).")]
        public string id;

        public StoreProductId() { }

        public StoreProductId(string store, string id)
        {
            this.store = store;
            this.id = id;
        }
    }

    /// <summary>One product in <see cref="BillingConfig"/>.</summary>
    [Serializable]
    public sealed class BillingProductConfig
    {
        [Tooltip("Id used in game code. Also the store product id unless a store below overrides it.")]
        public string id;

        public BillingProductType type;

        [Tooltip("Stores where the product has another id (e.g. a reverse-domain App Store id).")]
        public List<StoreProductId> storeIds = new List<StoreProductId>();

        [Tooltip("Price shown when the store has none (Steam, Stripe, Xsolla, the Editor), and the default price of the " +
                 "product when the config is imported into the admin panel. 0 = none.")]
        [Min(0)]
        public double fallbackPrice;

        [Tooltip("ISO 4217 currency of the fallback price, e.g. USD.")]
        public string fallbackCurrency;

        [Tooltip("Subscriptions: billing period shown when the store gives none (the App Store through Unity IAP, Steam, " +
                 "web stores, the Editor), ISO 8601: P1W, P1M, P3M, P6M or P1Y. Empty = none. Comes with the admin " +
                 "panel's config.")]
        public string fallbackPeriod;

        public BillingProductConfig() { }

        public BillingProductConfig(string id, BillingProductType type, params StoreProductId[] storeIds)
        {
            this.id = id;
            this.type = type;
            this.storeIds = storeIds?.ToList() ?? new List<StoreProductId>();
        }

        /// <summary>Sets the fallback price (fluent, for configs built in code).</summary>
        public BillingProductConfig WithFallbackPrice(double price, string currency)
        {
            fallbackPrice = price;
            fallbackCurrency = currency;
            return this;
        }

        /// <summary>Sets the fallback subscription period, ISO 8601 such as <c>P1M</c> (fluent, for configs built in code).</summary>
        public BillingProductConfig WithFallbackPeriod(string period)
        {
            fallbackPeriod = BillingPeriods.Normalize(period);
            return this;
        }

        /// <summary>True when a valid fallback subscription period is set.</summary>
        public bool HasFallbackPeriod => BillingPeriods.Normalize(fallbackPeriod).Length > 0;

        /// <summary>True when a fallback price and its currency are set.</summary>
        public bool HasFallbackPrice => fallbackPrice > 0 && !string.IsNullOrEmpty(fallbackCurrency);

        /// <summary>The fallback price as text, e.g. <c>0.99 USD</c>; empty without one.</summary>
        public string FallbackPriceText =>
            HasFallbackPrice ? fallbackPrice.ToString("0.00", CultureInfo.InvariantCulture) + " " + fallbackCurrency.ToUpperInvariant() : "";

        /// <summary>Sets the product's id in one store (fluent, for configs built in code).</summary>
        public BillingProductConfig WithStoreId(string store, string storeProductId)
        {
            storeIds = storeIds ?? new List<StoreProductId>();
            storeIds.RemoveAll(s => s?.store == store);
            storeIds.Add(new StoreProductId(store, storeProductId));
            return this;
        }

        /// <summary>Product id in a store (and in the server catalog of that store's build).</summary>
        public string StoreId(string store)
        {
            var match = storeIds?.FirstOrDefault(s => s != null && s.store == store && !string.IsNullOrEmpty(s.id));
            return match?.id ?? id;
        }

        /// <summary>True when <paramref name="storeId"/> is this product in any store. Empty ids never match.</summary>
        public bool Matches(string storeId)
        {
            return !string.IsNullOrEmpty(storeId) && AllIds().Contains(storeId);
        }

        /// <summary>The product's own id and every per-store id.</summary>
        public IEnumerable<string> AllIds()
        {
            var ids = new List<string>();
            if (!string.IsNullOrEmpty(id))
                ids.Add(id);
            if (storeIds != null)
                ids.AddRange(storeIds.Where(s => s != null && !string.IsNullOrEmpty(s.id)).Select(s => s.id));
            return ids.Distinct();
        }
    }
}
