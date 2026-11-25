// ITelegramWebApp for a WebGL build running as a Telegram Mini App: Telegram.WebApp.openInvoice through
// TelegramWebApp.jslib. The page must load https://telegram.org/js/telegram-web-app.js (add it to the WebGL template).
//      var deps = new BillingDependencies { UserIdProvider = ... };
//      deps.Store = new TelegramStoreAdapter(config, TelegramWebAppBridge.Create());
//      var billing = Billing.Create(config, deps);
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using UnityEngine;

namespace Purser.Samples
{
    public sealed class TelegramWebAppBridge : MonoBehaviour, ITelegramWebApp
    {
        private const string ObjectName = "PurserTelegramWebApp";
        private readonly Dictionary<string, Action<string>> m_Open = new Dictionary<string, Action<string>>();

#if UNITY_WEBGL && !UNITY_EDITOR
        [DllImport("__Internal")] private static extern void PurserTelegramOpenInvoice(string url, string gameObject);
#else
        private static void PurserTelegramOpenInvoice(string url, string gameObject) =>
            Debug.LogWarning("Telegram invoices open only in a WebGL build inside Telegram: " + url);
#endif

        /// <summary>The bridge on a GameObject of its own (the jslib answers it with SendMessage).</summary>
        public static TelegramWebAppBridge Create()
        {
            var existing = GameObject.Find(ObjectName);
            if (existing != null && existing.TryGetComponent(out TelegramWebAppBridge bridge))
                return bridge;
            var go = new GameObject(ObjectName);
            DontDestroyOnLoad(go);
            return go.AddComponent<TelegramWebAppBridge>();
        }

        public void OpenInvoice(string url, Action<string> closed)
        {
            m_Open[url] = closed;
            PurserTelegramOpenInvoice(url, gameObject.name);
        }

        /// <summary>Called by the jslib: <c>"&lt;url&gt;\n&lt;status&gt;"</c>.</summary>
        public void OnInvoiceClosed(string message)
        {
            var parts = (message ?? "").Split('\n');
            if (parts.Length < 2 || !m_Open.TryGetValue(parts[0], out var closed))
                return;
            m_Open.Remove(parts[0]);
            closed?.Invoke(parts[1]);
        }
    }
}
