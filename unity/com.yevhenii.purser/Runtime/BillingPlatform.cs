using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Where the build sells. Matches the platforms of the billing server (<c>game_app.platform</c>):
    /// Google Play (<c>android</c>), App Store (<c>ios</c>), Amazon Appstore (<c>amazon</c>), Steam, web.
    /// Unity IAP covers Google and Apple; the others need an <see cref="IStoreAdapter"/> in
    /// <see cref="BillingDependencies.Store"/> (<see cref="AmazonStoreAdapter"/> for Amazon,
    /// <see cref="SteamStoreAdapter"/> for Steam, <see cref="XsollaStoreAdapter"/> or your own for web).
    /// </summary>
    public enum BillingPlatform
    {
        /// <summary>Platform without a supported store (Editor, Standalone).</summary>
        Unsupported,
        Google,
        Apple,
        /// <summary>Amazon Appstore (Fire OS / Android). Use <see cref="AmazonStoreAdapter"/>.</summary>
        Amazon,
        /// <summary>Steam microtransactions. Use <see cref="SteamStoreAdapter"/>.</summary>
        Steam,
        /// <summary>Browser / web shop. Provide your own <see cref="IStoreAdapter"/> that also implements
        /// <see cref="IStoreNameProvider"/> (<c>stripe</c> or <c>xsolla</c>).</summary>
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
    /// Optional interface of an <see cref="IStoreAdapter"/> whose store is not implied by its platform
    /// (a web adapter selling through Stripe or Xsolla): the billing server route is <c>/verify/{StoreName}</c>.
    /// </summary>
    public interface IStoreNameProvider
    {
        /// <summary>Store id of the billing server, e.g. <c>stripe</c>.</summary>
        string StoreName { get; }
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

        /// <summary>Store id of the billing server for a platform (<c>/verify/{store}</c>); null for
        /// <see cref="BillingPlatform.Web"/> (the adapter names it, see <see cref="IStoreNameProvider"/>)
        /// and <see cref="BillingPlatform.Unsupported"/>.</summary>
        public static string StoreName(this BillingPlatform platform)
        {
            switch (platform)
            {
                case BillingPlatform.Google: return "google";
                case BillingPlatform.Apple: return "apple";
                case BillingPlatform.Amazon: return "amazon";
                case BillingPlatform.Steam: return "steam";
                default: return null;
            }
        }

        /// <summary>Store segment of the server route: <c>/verify/google</c>, <c>/verify/apple</c>, <c>/verify/amazon</c>, ...
        /// Google for platforms without a store of their own (kept for compatibility).</summary>
        public static string RouteName(this BillingPlatform platform)
        {
            return platform.StoreName() ?? "google";
        }
    }
}
