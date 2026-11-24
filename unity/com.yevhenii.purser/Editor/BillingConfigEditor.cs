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
            // What the Editor runs with: the editor profile applied (the copy, ping and admin page use it too). The copy
            // is a ScriptableObject: destroyed after each repaint.
            var effective = Effective(config, out var profileProblem);
            try
            {
                DrawTools(config, effective, profileProblem);
            }
            finally
            {
                if (effective != config)
                    DestroyImmediate(effective);
            }
        }

        private void DrawTools(BillingConfig config, BillingConfig effective, string profileProblem)
        {
            DrawProfiles(config, effective, profileProblem);

            using (new EditorGUILayout.HorizontalScope())
            {
                if (GUILayout.Button("Validate"))
                    m_Problems = config.Validate();
                using (new EditorGUI.DisabledScope(m_Ping != null))
                {
                    if (GUILayout.Button(m_Ping != null ? "Pinging..." : "Ping server"))
                        StartPing(effective);
                }
                using (new EditorGUI.DisabledScope(AdminUrl(effective) == null))
                {
                    if (GUILayout.Button(new GUIContent("Open admin panel", "Opens the billing server's admin page in the browser")))
                        Application.OpenURL(AdminUrl(effective));
                }
            }
            using (new EditorGUILayout.HorizontalScope())
            {
                if (GUILayout.Button(new GUIContent("Copy config for admin panel",
                        "Copies the builds and products as JSON: paste them in the admin panel (Add an app → Paste from Unity, " +
                        "or an app's Products tab → Import from Unity)")))
                {
                    EditorGUIUtility.systemCopyBuffer = effective.ToJson();
                    m_Copied = $"Copied {effective.builds?.Count ?? 0} builds and {effective.products?.Count ?? 0} products" +
                               (effective.ProfileName.Length > 0 ? $" of the profile '{effective.ProfileName}'." : ".");
                }
                if (GUILayout.Button(new GUIContent("Paste config from admin panel",
                        "Shows what the Unity config JSON copied from the admin panel changes, then applies it")))
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

            var builds = (effective.builds ?? new List<BillingBuild>()).Where(b => b != null && !string.IsNullOrEmpty(b.store)).ToList();
            EditorGUILayout.HelpBox(builds.Count == 0
                    ? $"No builds: every store uses '{Application.identifier}' as its app id on the server."
                    : "App ids sent to the server: " + string.Join(", ", builds.Select(b => $"{b.store} '{effective.AppIdFor(b.store)}'")) +
                      $". Other stores use '{Application.identifier}'. The builds and product ids must exist on the server: " +
                      "copy this config into the admin panel.",
                MessageType.None);
        }

        /// <summary>The config with the editor profile applied; the config itself (and the problem) when that profile
        /// does not exist.</summary>
        internal static BillingConfig Effective(BillingConfig config, out string problem)
        {
            problem = null;
            var profile = config.FindProfile(config.editorProfile?.Trim());
            if (profile != null)
                return config.WithProfile(profile);
            if (!string.IsNullOrWhiteSpace(config.editorProfile))
                problem = $"Editor profile '{config.editorProfile.Trim()}' does not exist: play mode fails to start billing.";
            return config;
        }

        private static void DrawProfiles(BillingConfig config, BillingConfig effective, string problem)
        {
            if (config.profiles == null || config.profiles.Count == 0)
                return;
            if (problem != null)
            {
                EditorGUILayout.HelpBox(problem, MessageType.Error);
                return;
            }
            string Name(string n) => string.IsNullOrWhiteSpace(n) ? "none" : n.Trim();
            EditorGUILayout.HelpBox(
                $"Editor: {Name(config.editorProfile)} ({effective.serverBaseUrl}). Development Builds: {Name(config.developmentProfile)}. " +
                $"Release builds: {Name(config.releaseProfile)}. Copy, paste, ping and the admin panel use the Editor's profile; " +
                "BillingConfig.UseProfile picks another at run time.",
                MessageType.Info);
        }

        /// <summary>The admin page next to the HTTP API: <c>{serverBaseUrl}/admin</c>; null until the URL is set.</summary>
        internal static string AdminUrl(BillingConfig config)
        {
            var url = config.serverBaseUrl?.Trim().TrimEnd('/');
            if (string.IsNullOrEmpty(url) || url.Contains(BillingConfig.ServerUrlPlaceholder) ||
                !url.StartsWith("https://", System.StringComparison.OrdinalIgnoreCase) || !url.EndsWith("/route"))
                return null;
            return url + "/admin";
        }

        private void PasteConfig(BillingConfig config)
        {
            var json = EditorGUIUtility.systemCopyBuffer;
            var profile = config.FindProfile(config.editorProfile?.Trim())?.name;
            try
            {
                if (profile != null)
                    config.PreviewJsonForProfile(json, profile);
                else
                    config.PreviewJson(json);
            }
            catch (System.Exception e)
            {
                m_Problems = new List<string> { "The clipboard does not contain the admin panel's Unity config: " + e.Message };
                return;
            }
            m_Copied = null;
            ConfigSyncWindow.Open(config, profile, json, problems =>
            {
                m_Problems = problems;
                Repaint();
            });
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

    /// <summary>
    /// What pasting the admin panel's config changes, before it is applied: additive (add and update) or a mirror of the
    /// admin panel (also removes what it does not have). Applying is one undo step.
    /// </summary>
    internal sealed class ConfigSyncWindow : EditorWindow
    {
        private BillingConfig m_Config;
        /// <summary>The profile the admin panel's environment belongs to; null = the config itself.</summary>
        private string m_Profile;
        private string m_Json;
        private System.Action<List<string>> m_Done;
        private ConfigMergeMode m_Mode = ConfigMergeMode.Additive;
        private List<ConfigChange> m_Changes = new List<ConfigChange>();
        private string m_Error;
        private Vector2 m_Scroll;

        private static readonly GUIContent[] ModeLabels =
        {
            new GUIContent("Add and update", "Keeps builds, products and store ids that only this config has"),
            new GUIContent("Match the admin panel", "Also removes builds, products and store ids the admin panel does not have"),
        };

        public static void Open(BillingConfig config, string profile, string json, System.Action<List<string>> done)
        {
            var w = CreateInstance<ConfigSyncWindow>();
            w.titleContent = new GUIContent(profile == null ? "Sync from admin panel" : $"Sync profile {profile} from admin panel");
            w.m_Config = config;
            w.m_Profile = profile;
            w.m_Json = json;
            w.m_Done = done;
            w.minSize = new Vector2(560, 340);
            w.Refresh();
            w.ShowUtility();
        }

        private void Refresh()
        {
            try
            {
                m_Changes = m_Profile == null ? m_Config.PreviewJson(m_Json, m_Mode) : m_Config.PreviewJsonForProfile(m_Json, m_Profile, m_Mode);
                m_Error = null;
            }
            catch (System.Exception e)
            {
                m_Changes = new List<ConfigChange>();
                m_Error = e.Message;
            }
        }

        private void OnGUI()
        {
            if (m_Config == null)
            {
                Close();
                return;
            }
            EditorGUILayout.Space();
            var mode = (ConfigMergeMode)GUILayout.Toolbar((int)m_Mode, ModeLabels);
            if (mode != m_Mode)
            {
                m_Mode = mode;
                Refresh();
            }
            EditorGUILayout.HelpBox(m_Mode == ConfigMergeMode.Additive
                    ? "Adds new builds and products and updates the ones both sides have. What only this config has stays."
                    : "Makes the builds and products match the admin panel: what it does not have is removed. " +
                      "Fallback prices stay where the admin panel has no price.",
                m_Mode == ConfigMergeMode.Additive ? MessageType.Info : MessageType.Warning);

            if (m_Profile != null)
                EditorGUILayout.HelpBox($"The Editor runs with the profile '{m_Profile}': the server URL, builds and store ids that differ " +
                                        "go to that profile, the catalog to the config every profile shares.", MessageType.None);
            if (m_Error != null)
                EditorGUILayout.HelpBox(m_Error, MessageType.Error);
            else if (m_Changes.Count == 0)
                EditorGUILayout.HelpBox($"{m_Config.name} already matches: nothing to change.", MessageType.None);
            else
            {
                var added = m_Changes.Count(c => c.Kind == ConfigChangeKind.Added);
                var updated = m_Changes.Count(c => c.Kind == ConfigChangeKind.Updated);
                var removed = m_Changes.Count(c => c.Kind == ConfigChangeKind.Removed);
                EditorGUILayout.LabelField($"{m_Config.name}: {added} to add, {updated} to update, {removed} to remove", EditorStyles.boldLabel);
                m_Scroll = EditorGUILayout.BeginScrollView(m_Scroll);
                foreach (var c in m_Changes)
                {
                    using (new EditorGUILayout.HorizontalScope())
                    {
                        var (label, color) = c.Kind == ConfigChangeKind.Added ? ("Add", new Color(0.35f, 0.75f, 0.4f))
                            : c.Kind == ConfigChangeKind.Removed ? ("Remove", new Color(0.9f, 0.4f, 0.35f))
                            : ("Update", new Color(0.95f, 0.75f, 0.3f));
                        var style = new GUIStyle(EditorStyles.miniBoldLabel) { normal = { textColor = color } };
                        GUILayout.Label(label, style, GUILayout.Width(52));
                        GUILayout.Label(c.Target, EditorStyles.boldLabel, GUILayout.Width(170));
                        GUILayout.Label(c.Details, EditorStyles.wordWrappedLabel);
                    }
                }
                EditorGUILayout.EndScrollView();
            }

            GUILayout.FlexibleSpace();
            using (new EditorGUILayout.HorizontalScope())
            {
                GUILayout.FlexibleSpace();
                if (GUILayout.Button("Cancel", GUILayout.Width(90)))
                    Close();
                using (new EditorGUI.DisabledScope(m_Error != null || m_Changes.Count == 0))
                {
                    if (GUILayout.Button("Apply", GUILayout.Width(90)))
                        Apply();
                }
            }
            EditorGUILayout.Space();
        }

        private void Apply()
        {
            Undo.RecordObject(m_Config, "Sync billing config from admin panel");
            if (m_Profile == null)
                m_Config.MergeJson(m_Json, m_Mode);
            else
                m_Config.MergeJsonIntoProfile(m_Json, m_Profile, m_Mode);
            EditorUtility.SetDirty(m_Config);
            m_Done?.Invoke(m_Config.Validate());
            Close();
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
