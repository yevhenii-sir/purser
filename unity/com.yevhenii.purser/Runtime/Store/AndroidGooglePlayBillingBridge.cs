using System;
#if UNITY_ANDROID && !UNITY_EDITOR
using UnityEngine;
using UnityEngine.Scripting;
#endif

namespace Purser
{
    /// <summary>
    /// <see cref="IGooglePlayBillingBridge"/> on the package's Android plugin
    /// (<c>Runtime/Plugins/Android/PurserGoogleBilling.java</c>), which calls the Google Play Billing Library that
    /// Unity IAP 5.4+ adds to the build (Billing Library 9 with Unity IAP 5.4; the plugin works with 8 and later). The
    /// plugin is left out of builds without Unity IAP 5.4+; then, in the Editor and on other platforms,
    /// <see cref="IsAvailable"/> is false.
    /// </summary>
    public sealed class AndroidGooglePlayBillingBridge : IGooglePlayBillingBridge
    {
        private const string PluginClass = "com.yevhenii.purser.PurserGoogleBilling";

#if UNITY_ANDROID && !UNITY_EDITOR
        private bool? m_Available;

        /// <inheritdoc/>
        public bool IsAvailable
        {
            get
            {
                if (m_Available == null)
                {
                    try
                    {
                        using (new AndroidJavaClass(PluginClass))
                        using (new AndroidJavaClass("com.android.billingclient.api.BillingClient"))
                            m_Available = true;
                    }
                    catch (Exception)
                    {
                        // Not in the build (no Unity IAP 5.4+), or stripped by minification without keep rules.
                        m_Available = false;
                    }
                }
                return m_Available.Value;
            }
        }

        /// <inheritdoc/>
        public void QueryOffers(string productId, Action<string> onResult)
        {
            using (var player = new AndroidJavaClass("com.unity3d.player.UnityPlayer"))
            using (var activity = player.GetStatic<AndroidJavaObject>("currentActivity"))
            using (var plugin = new AndroidJavaClass(PluginClass))
                plugin.CallStatic("queryOffers", activity, productId, new Callback(onResult));
        }

        /// <inheritdoc/>
        public void Purchase(GooglePlayBillingFlow flow, Action<string> onResult)
        {
            using (var player = new AndroidJavaClass("com.unity3d.player.UnityPlayer"))
            using (var activity = player.GetStatic<AndroidJavaObject>("currentActivity"))
            using (var plugin = new AndroidJavaClass(PluginClass))
                plugin.CallStatic("purchase", activity, flow.ProductId, flow.OfferToken, flow.OldProductId ?? "",
                    (int)flow.ReplacementMode, flow.ObfuscatedAccountId ?? "", new Callback(onResult));
        }

        /// <summary><c>PurserGoogleBilling.Callback</c>: called once, on Android's main thread.</summary>
        private sealed class Callback : AndroidJavaProxy
        {
            private readonly Action<string> m_OnResult;

            public Callback(Action<string> onResult) : base(PluginClass + "$Callback")
            {
                m_OnResult = onResult;
            }

            // Called by name from Java (AndroidJavaProxy): kept from managed code stripping.
            [Preserve]
            public void onResult(string json) => m_OnResult(json);
        }
#else
        /// <inheritdoc/>
        public bool IsAvailable => false;

        /// <inheritdoc/>
        public void QueryOffers(string productId, Action<string> onResult) =>
            throw new BillingException(GooglePlayOfferClient.NotSupportedMessage);

        /// <inheritdoc/>
        public void Purchase(GooglePlayBillingFlow flow, Action<string> onResult) =>
            throw new BillingException(GooglePlayOfferClient.NotSupportedMessage);
#endif
    }
}
