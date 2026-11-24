using System;
using System.Collections.Generic;
using UnityEngine;

namespace Purser
{
    /// <summary>A product's id in one store for one <see cref="BillingProfile"/>, e.g. a test-mode Stripe Price id.</summary>
    [Serializable]
    public sealed class ProductIdOverride
    {
        [Tooltip("Id of the product in the config (its id in game code).")]
        public string product;

        [Tooltip("Store id: google, apple, amazon, steam, stripe, xsolla (see BillingStores).")]
        public string store;

        [Tooltip("The product's id in that store for this profile.")]
        public string id;

        public ProductIdOverride() { }

        public ProductIdOverride(string product, string store, string id)
        {
            this.product = product;
            this.store = store;
            this.id = id;
        }
    }

    /// <summary>
    /// An environment of the config (dev, staging, prod): its billing server and what differs there. Empty fields and
    /// stores without a row come from the config itself, so the catalog and the behaviour are written once. Which
    /// profile a game uses: <see cref="BillingConfig.editorProfile"/>, <see cref="BillingConfig.developmentProfile"/>,
    /// <see cref="BillingConfig.releaseProfile"/> or <see cref="BillingConfig.UseProfile"/>.
    /// </summary>
    [Serializable]
    public sealed class BillingProfile
    {
        [Tooltip("Name the config's profile fields and BillingConfig.UseProfile refer to, e.g. dev or prod.")]
        public string name;

        [Tooltip("Base URL of this environment's billing server. Empty = the config's.")]
        public string serverBaseUrl = "";

        [Tooltip("Response signing public key of this environment's builds. Empty = the config's.")]
        public string responseSigningPublicKey = "";

        [Tooltip("App ids of this environment's builds, per store. Stores without a row use the config's builds.")]
        public List<BillingBuild> builds = new List<BillingBuild>();

        [Tooltip("Product ids that differ in this environment (e.g. test-mode Stripe Price ids). Others come from the products above.")]
        public List<ProductIdOverride> productIds = new List<ProductIdOverride>();

        [Tooltip("A test environment: Validate and release builds warn when the release profile is one.")]
        public bool isTest;

        public BillingProfile() { }

        public BillingProfile(string name)
        {
            this.name = name;
        }

        /// <summary>Sets the server (fluent, for configs built in code).</summary>
        public BillingProfile WithServer(string url)
        {
            serverBaseUrl = url;
            return this;
        }

        /// <summary>Adds or replaces the build of a store (fluent).</summary>
        public BillingProfile WithBuild(string store, string appId)
        {
            builds = builds ?? new List<BillingBuild>();
            builds.RemoveAll(b => b?.store == store);
            builds.Add(new BillingBuild(store, appId));
            return this;
        }

        /// <summary>Adds or replaces a product's id in a store (fluent).</summary>
        public BillingProfile WithProductId(string product, string store, string id)
        {
            productIds = productIds ?? new List<ProductIdOverride>();
            productIds.RemoveAll(o => o?.product == product && o.store == store);
            productIds.Add(new ProductIdOverride(product, store, id));
            return this;
        }

        /// <summary>Marks the profile as a test environment (fluent).</summary>
        public BillingProfile AsTest()
        {
            isTest = true;
            return this;
        }
    }
}
