using System.Collections.Generic;
using System.Linq;
using UnityEditor;
using UnityEngine;
using UnityEngine.Networking;

namespace Purser.Editor
{
    [CustomEditor(typeof(BillingConfig))]
    public sealed class BillingConfigEditor : UnityEditor.Editor
    {
        private List<string> m_Problems;
        private string m_PingResult;
        private string m_Copied;
        private UnityWebRequestAsyncOperation m_Ping;

        public override void OnInspectorGUI()
        {
            var config = (BillingConfig)target;
            DrawDefaultInspector();
            EditorGUILayout.Space();

            using (new EditorGUILayout.HorizontalScope())
            {
                if (GUILayout.Button("Validate"))
                    m_Problems = config.Validate();
                using (new EditorGUI.DisabledScope(m_Ping != null))
                {
                    if (GUILayout.Button(m_Ping != null ? "Pinging..." : "Ping server"))
                        StartPing(config);
                }
            }
            using (new EditorGUILayout.HorizontalScope())
            {
                if (GUILayout.Button(new GUIContent("Copy config for admin panel",
                        "Copies the builds and products as JSON: paste them in the admin panel (Add an app → Paste from Unity, " +
                        "or an app's Products tab → Import from Unity)")))
                {
                    EditorGUIUtility.systemCopyBuffer = config.ToJson();
                    m_Copied = $"Copied {config.builds?.Count ?? 0} builds and {config.products?.Count ?? 0} products.";
                }
                if (GUILayout.Button(new GUIContent("Paste config from admin panel",
                        "Applies the Unity config JSON copied from the admin panel")))
                    PasteConfig(config);
            }

            if (!string.IsNullOrEmpty(m_Copied))
                EditorGUILayout.HelpBox(m_Copied, MessageType.Info);
            if (m_Problems != null)
            {
                if (m_Problems.Count == 0)
                    EditorGUILayout.HelpBox("Config looks good.", MessageType.Info);
                foreach (var problem in m_Problems)
                    EditorGUILayout.HelpBox(problem, MessageType.Warning);
            }
            if (!string.IsNullOrEmpty(m_PingResult))
                EditorGUILayout.HelpBox(m_PingResult, m_PingResult.StartsWith("OK") ? MessageType.Info : MessageType.Error);

            var builds = (config.builds ?? new List<BillingBuild>()).Where(b => b != null && !string.IsNullOrEmpty(b.store)).ToList();
            EditorGUILayout.HelpBox(builds.Count == 0
                    ? $"No builds: every store uses '{Application.identifier}' as its app id on the server."
                    : "App ids sent to the server: " + string.Join(", ", builds.Select(b => $"{b.store} '{config.AppIdFor(b.store)}'")) +
                      $". Other stores use '{Application.identifier}'. The builds and product ids must exist on the server: " +
                      "copy this config into the admin panel.",
                MessageType.None);
        }

        private void PasteConfig(BillingConfig config)
        {
            Undo.RecordObject(config, "Paste billing config");
            try
            {
                config.ApplyJson(EditorGUIUtility.systemCopyBuffer);
                EditorUtility.SetDirty(config);
                m_Problems = config.Validate();
            }
            catch (System.Exception e)
            {
                m_Problems = new List<string> { "The clipboard does not contain the admin panel's Unity config: " + e.Message };
            }
        }

        private void StartPing(BillingConfig config)
        {
            m_PingResult = null;
            var request = UnityWebRequest.Get(config.serverBaseUrl.TrimEnd('/') + "/healthz");
            request.timeout = Mathf.Max(1, config.requestTimeoutSeconds);
            m_Ping = request.SendWebRequest();
            m_Ping.completed += _ =>
            {
                m_PingResult = request.result == UnityWebRequest.Result.Success
                    ? $"OK {request.responseCode}: {request.downloadHandler.text}"
                    : $"Failed: {request.error} {request.downloadHandler?.text}";
                request.Dispose();
                m_Ping = null;
                Repaint();
            };
        }
    }

    /// <summary>One line per build or per-store product id: a store popup and the id.</summary>
    [CustomPropertyDrawer(typeof(BillingBuild))]
    [CustomPropertyDrawer(typeof(StoreProductId))]
    public sealed class StoreRowDrawer : PropertyDrawer
    {
        private const string Other = "Other…";

        public override float GetPropertyHeight(SerializedProperty property, GUIContent label) => EditorGUIUtility.singleLineHeight;

        public override void OnGUI(Rect position, SerializedProperty property, GUIContent label)
        {
            var store = property.FindPropertyRelative("store");
            var id = property.FindPropertyRelative("appId") ?? property.FindPropertyRelative("id");
            EditorGUI.BeginProperty(position, label, property);
            const float storeWidth = 110f;
            var storeRect = new Rect(position.x, position.y, storeWidth, position.height);
            var idRect = new Rect(position.x + storeWidth + 4f, position.y, position.width - storeWidth - 4f, position.height);

            var options = BillingStores.All.ToList();
            var known = options.IndexOf(store.stringValue);
            options.Add(Other);
            if (known < 0 && !string.IsNullOrEmpty(store.stringValue))
            {
                // A store this package version does not list: edit it as text.
                store.stringValue = EditorGUI.TextField(storeRect, store.stringValue);
            }
            else
            {
                var picked = EditorGUI.Popup(storeRect, known < 0 ? 0 : known, options.ToArray());
                store.stringValue = picked == options.Count - 1 ? "other" : options[picked];
            }
            id.stringValue = EditorGUI.TextField(idRect, id.stringValue);
            EditorGUI.EndProperty();
        }
    }

    internal static class BillingMenu
    {
        [MenuItem("Tools/Purser/Create Config")]
        private static void CreateConfig()
        {
            var path = EditorUtility.SaveFilePanelInProject("Create Billing Config", "BillingConfig", "asset",
                "Where to save the billing config?");
            if (string.IsNullOrEmpty(path))
                return;
            var config = ScriptableObject.CreateInstance<BillingConfig>();
            AssetDatabase.CreateAsset(config, path);
            AssetDatabase.SaveAssets();
            Selection.activeObject = config;
            EditorGUIUtility.PingObject(config);
        }

        [MenuItem("Tools/Purser/Add Bootstrap To Scene")]
        private static void AddBootstrap()
        {
#if UNITY_2023_1_OR_NEWER
            var existing = Object.FindFirstObjectByType<BillingBootstrap>();
#else
            var existing = Object.FindObjectOfType<BillingBootstrap>();
#endif
            if (existing != null)
            {
                Selection.activeObject = existing.gameObject;
                return;
            }
            var go = new GameObject("Billing");
            go.AddComponent<BillingBootstrap>();
            Undo.RegisterCreatedObjectUndo(go, "Add Billing Bootstrap");
            Selection.activeObject = go;
        }
    }
}
