using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;

namespace Purser.Editor
{
    /// <summary>
    /// Keeps the Android plugin of Google Play offers (<c>Runtime/Plugins/Android/PurserGoogleBilling.java</c>) out of
    /// builds without Unity IAP 5.4 or later: it is compiled against the Google Play Billing Library that Unity IAP adds
    /// to the Gradle project, so without it the Gradle build would fail. Without the plugin
    /// <see cref="IBillingService.GoogleOffersSupported"/> is false.
    /// </summary>
    [InitializeOnLoad]
    internal sealed class PurserGoogleBillingPlugin : IPreprocessBuildWithReport
    {
        /// <summary>The GUID of the plugin's <c>.meta</c> (derived from its path by tools/generate_unity_meta.py).</summary>
        internal const string PluginGuid = "ea5f71fe73d1da04220272e1f8b4c454";

        /// <summary>The project has Unity IAP 5.4 or later (<c>versionDefines</c> of Purser.Editor.asmdef).</summary>
        internal static bool HasUnityIap
        {
            get
            {
#if PURSER_UNITY_IAP5
                return true;
#else
                return false;
#endif
            }
        }

        static PurserGoogleBillingPlugin()
        {
            // The asset database may not be ready while the editor loads: also set before every build.
            EditorApplication.delayCall += Register;
        }

        /// <inheritdoc/>
        public int callbackOrder => 0;

        /// <inheritdoc/>
        public void OnPreprocessBuild(BuildReport report) => Register();

        private static void Register()
        {
            var path = AssetDatabase.GUIDToAssetPath(PluginGuid);
            if (string.IsNullOrEmpty(path) || !(AssetImporter.GetAtPath(path) is PluginImporter importer))
                return;
            importer.SetIncludeInBuildDelegate(_ => HasUnityIap);
        }
    }
}
