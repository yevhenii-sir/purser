using System;
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

    /// <summary>One product in <see cref="BillingConfig"/>.</summary>
    [Serializable]
    public sealed class BillingProductConfig
    {
        [Tooltip("Id used in game code. Also the store product id unless overridden below.")]
        public string id;

        public BillingProductType type;

        [Tooltip("Google Play product id, if it differs from Id.")]
        public string googleId;

        [Tooltip("App Store product id, if it differs from Id.")]
        public string appleId;

        [Tooltip("Amazon Appstore SKU, if it differs from Id (for subscriptions: the term SKU).")]
        public string amazonId;

        public BillingProductConfig() { }

        public BillingProductConfig(string id, BillingProductType type, string googleId = null, string appleId = null, string amazonId = null)
        {
            this.id = id;
            this.type = type;
            this.googleId = googleId;
            this.appleId = appleId;
            this.amazonId = amazonId;
        }

        /// <summary>Product id in the store (and in the billing server catalog) for a platform.</summary>
        public string StoreId(BillingPlatform platform)
        {
            if (platform == BillingPlatform.Google && !string.IsNullOrEmpty(googleId))
                return googleId;
            if (platform == BillingPlatform.Apple && !string.IsNullOrEmpty(appleId))
                return appleId;
            if (platform == BillingPlatform.Amazon && !string.IsNullOrEmpty(amazonId))
                return amazonId;
            return id;
        }

        /// <summary>True when <paramref name="storeId"/> is this product on any platform.</summary>
        public bool Matches(string storeId)
        {
            return storeId == id || storeId == googleId || storeId == appleId || storeId == amazonId;
        }
    }
}
