using UnityEditor;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEngine;

namespace Purser.Editor
{
    /// <summary>
    /// Before a player build, checks the profile every <see cref="BillingConfig"/> of the project will run with: a
    /// profile that does not exist stops billing at startup (an error), a release build on a test profile goes to a
    /// test server (a warning).
    /// </summary>
    internal sealed class PurserProfileBuildCheck : IPreprocessBuildWithReport
    {
        /// <inheritdoc/>
        public int callbackOrder => 0;

        /// <inheritdoc/>
        public void OnPreprocessBuild(BuildReport report)
        {
            var development = (report.summary.options & BuildOptions.Development) != 0;
            foreach (var guid in AssetDatabase.FindAssets("t:" + nameof(BillingConfig)))
            {
                var path = AssetDatabase.GUIDToAssetPath(guid);
                var config = AssetDatabase.LoadAssetAtPath<BillingConfig>(path);
                if (config == null)
                    continue;
                var name = (development ? config.developmentProfile : config.releaseProfile)?.Trim();
                if (string.IsNullOrEmpty(name))
                    continue;
                var profile = config.FindProfile(name);
                var kind = development ? "Development" : "release";
                if (profile == null)
                    Debug.LogError($"[Purser] {path}: the {kind} profile '{name}' does not exist; billing fails to start in this build.", config);
                else if (!development && profile.isTest)
                    Debug.LogWarning($"[Purser] {path}: this release build uses the test profile '{name}' ({profile.serverBaseUrl}).", config);
            }
        }
    }
}
