using System;
using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Opens a payment page (Xsolla Pay Station, Stripe Checkout). Android: Chrome Custom Tabs when the app has
    /// <c>androidx.browser</c> (the package asks the External Dependency Manager for it), otherwise the default browser.
    /// iOS and elsewhere: <c>Application.OpenURL</c> (Safari: Apple wants purchases outside the App Store in the
    /// browser). The page returns to the game with an app link (<see cref="XsollaOptions.ReturnUrl"/>).
    /// </summary>
    public static class PurserBrowser
    {
        /// <summary>Opens <paramref name="url"/> as described above; https pages only.</summary>
        public static void Open(string url)
        {
            if (string.IsNullOrEmpty(url) || !url.StartsWith("https://", StringComparison.OrdinalIgnoreCase))
                return;
#if UNITY_ANDROID && !UNITY_EDITOR
            try
            {
                using (var player = new AndroidJavaClass("com.unity3d.player.UnityPlayer"))
                using (var activity = player.GetStatic<AndroidJavaObject>("currentActivity"))
                using (var browser = new AndroidJavaClass("com.yevhenii.purser.PurserBrowser"))
                {
                    browser.CallStatic("open", activity, url);
                    return;
                }
            }
            catch (Exception e)
            {
                Debug.LogWarning("[Purser] Custom Tabs unavailable, opening the browser: " + e.Message);
            }
#endif
            Application.OpenURL(url);
        }
    }
}
