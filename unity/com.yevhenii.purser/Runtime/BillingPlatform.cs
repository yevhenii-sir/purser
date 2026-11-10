using System.Collections.Generic;
using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Where the build sells. Matches the platforms of the billing server (<c>game_app.platform</c>):
    /// Google Play (<c>android</c>), App Store (<c>ios</c>), Amazon Appstore (<c>amazon</c>), Steam, web.
    /// Unity IAP covers Google and Apple; the others need an <see cref="IStoreAdapter"/> in
    /// <see cref="BillingDependencies.Store"/> (<see cref="AmazonStoreAdapter"/> for Amazon,
    /// <see cref="SteamStoreAdapter"/> for Steam, <see cref="StripeStoreAdapter"/> or <see cref="XsollaStoreAdapter"/> for web).
    /// </summary>
    public enum BillingPlatform
    {
        /// <summary>Platform without a supported store (Editor, Standalone).</summary>
        Unsupported,
        /// <summary>Google Play (Android, Unity IAP).</summary>
        Google,
        /// <summary>App Store (iOS, tvOS, macOS, Unity IAP).</summary>
        Apple,
        /// <summary>Amazon Appstore (Fire OS / Android). Use <see cref="AmazonStoreAdapter"/>.</summary>
        Amazon,
        /// <summary>Steam microtransactions. Use <see cref="SteamStoreAdapter"/>.</summary>
        Steam,
        /// <summary>Browser / web shop: <see cref="StripeStoreAdapter"/>, <see cref="XsollaStoreAdapter"/> or your own
        /// <see cref="IStoreAdapter"/> (its <see cref="IStoreAdapter.Store"/> names the server's store).</summary>
        Web,
    }

    /// <summary>Which store an Android build talks to.</summary>
    public enum AndroidStore
    {
        /// <summary>Google Play, unless the app was installed by the Amazon Appstore.</summary>
        Auto,
        GooglePlay,
        AmazonAppstore,
    }

    /// <summary>
    /// Store ids of the billing server (<c>purser_core::registry</c>): the <c>/verify/{store}</c> route, the
    /// <see cref="BillingBuild.store"/> of a build and the key of a product's per-store id. Every
    /// <see cref="IStoreAdapter.Store"/> is one of them.
    /// </summary>
    public static class BillingStores
    {
        public const string Google = "google";
        public const string Apple = "apple";
        public const string Amazon = "amazon";
        public const string Steam = "steam";
        public const string Stripe = "stripe";
        public const string Xsolla = "xsolla";

        /// <summary>Stores the server knows, in the order the inspector lists them.</summary>
        public static readonly IReadOnlyList<string> All = new[] { Google, Apple, Amazon, Steam, Stripe, Xsolla };

        /// <summary>The store a platform implies; null for <see cref="BillingPlatform.Web"/> (Stripe or Xsolla:
        /// the adapter names it) and <see cref="BillingPlatform.Unsupported"/>.</summary>
        public static string Of(BillingPlatform platform)
        {
            switch (platform)
            {
                case BillingPlatform.Google: return Google;
                case BillingPlatform.Apple: return Apple;
                case BillingPlatform.Amazon: return Amazon;
                case BillingPlatform.Steam: return Steam;
                default: return null;
            }
        }
    }

    public static class BillingPlatforms
    {
        /// <summary>Store package of the Amazon Appstore (the installer of apps it delivers).</summary>
        public const string AmazonInstallerPackage = "com.amazon.venezia";

        /// <summary>Platform Unity IAP serves on this device (Google on every Android device).</summary>
        public static BillingPlatform Current
        {
            get
            {
                switch (Application.platform)
                {
                    case RuntimePlatform.Android:
                        return BillingPlatform.Google;
                    case RuntimePlatform.IPhonePlayer:
                    case RuntimePlatform.tvOS:
                    case RuntimePlatform.OSXPlayer:
                        return BillingPlatform.Apple;
                    default:
                        return BillingPlatform.Unsupported;
                }
            }
        }

        /// <summary>Platform for a config: <see cref="Current"/>, with Android builds resolved by
        /// <see cref="BillingConfig.androidStore"/> (Auto = Amazon when installed by the Amazon Appstore).</summary>
        public static BillingPlatform For(BillingConfig config)
        {
            var current = Current;
            if (current != BillingPlatform.Google || config == null)
                return current;
            switch (config.androidStore)
            {
                case AndroidStore.AmazonAppstore:
                    return BillingPlatform.Amazon;
                case AndroidStore.Auto:
                    return IsAmazonAppstoreInstall() ? BillingPlatform.Amazon : BillingPlatform.Google;
                default:
                    return BillingPlatform.Google;
            }
        }

        /// <summary>True when the Amazon Appstore installed this app (Android only).</summary>
        public static bool IsAmazonAppstoreInstall()
        {
#if UNITY_ANDROID && !UNITY_EDITOR
            try
            {
                using (var player = new AndroidJavaClass("com.unity3d.player.UnityPlayer"))
                using (var activity = player.GetStatic<AndroidJavaObject>("currentActivity"))
                using (var pm = activity.Call<AndroidJavaObject>("getPackageManager"))
                {
                    var installer = pm.Call<string>("getInstallerPackageName", activity.Call<string>("getPackageName"));
                    return installer == AmazonInstallerPackage;
                }
            }
            catch (System.Exception)
            {
                return false;
            }
#else
            return false;
#endif
        }
    }
}
