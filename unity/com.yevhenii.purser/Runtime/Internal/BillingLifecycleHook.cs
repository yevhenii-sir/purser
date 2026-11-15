using UnityEngine;

namespace Purser
{
    /// <summary>
    /// Hidden object the service creates in play mode: tells it when the app comes back to the foreground (customer
    /// info refresh) and ticks once a second (a changed user id of the user id provider is noticed).
    /// </summary>
    [AddComponentMenu("")]
    internal sealed class BillingLifecycleHook : MonoBehaviour
    {
        private const float TickSeconds = 1f;

        internal BillingService Service;
        private float m_NextTick;

        private void Update()
        {
            if (Time.unscaledTime < m_NextTick)
                return;
            m_NextTick = Time.unscaledTime + TickSeconds;
            Service?.OnTick();
        }

        private void OnApplicationPause(bool paused)
        {
            if (!paused)
                Service?.OnAppResumed();
        }

        private void OnApplicationFocus(bool focused)
        {
            if (focused)
                Service?.OnAppResumed();
        }

        /// <summary>Creates the hook for <paramref name="service"/>; survives scene loads.</summary>
        internal static BillingLifecycleHook Create(BillingService service)
        {
            var go = new GameObject("Purser") { hideFlags = HideFlags.HideAndDontSave };
            DontDestroyOnLoad(go);
            var hook = go.AddComponent<BillingLifecycleHook>();
            hook.Service = service;
            return hook;
        }

        /// <summary>Removes the hook's object.</summary>
        internal void Release()
        {
            Service = null;
            if (this == null || gameObject == null)
                return;
            if (Application.isPlaying)
                Destroy(gameObject);
            else
                DestroyImmediate(gameObject);
        }
    }
}
