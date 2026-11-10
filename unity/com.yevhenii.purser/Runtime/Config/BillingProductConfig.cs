using System;
using System.Collections.Generic;
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

        public BillingProductConfig() { }

        public BillingProductConfig(string id, BillingProductType type, params StoreProductId[] storeIds)
        {
            this.id = id;
            this.type = type;
            this.storeIds = storeIds?.ToList() ?? new List<StoreProductId>();
        }

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
