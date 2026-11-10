using System.Collections.Generic;
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
                if (GUILayout.Button(new GUIContent("Paste config from admin panel",
                        "Applies the Unity config JSON copied from the admin panel's Add game wizard")))
                    PasteConfig(config);
            }

            if (m_Problems != null)
            {
                if (m_Problems.Count == 0)
                    EditorGUILayout.HelpBox("Config looks good.", MessageType.Info);
                foreach (var problem in m_Problems)
                    EditorGUILayout.HelpBox(problem, MessageType.Warning);
            }
            if (!string.IsNullOrEmpty(m_PingResult))
                EditorGUILayout.HelpBox(m_PingResult, m_PingResult.StartsWith("OK") ? MessageType.Info : MessageType.Error);

            EditorGUILayout.HelpBox(
                $"App ids sent to the server: Android '{config.AppIdFor(BillingPlatform.Google)}', iOS '{config.AppIdFor(BillingPlatform.Apple)}'. " +
                "They and every product id must be registered on the billing server (server/seed.py).",
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
