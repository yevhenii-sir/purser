using System.IO;
using UnityEditor.Build;
using UnityEditor.Build.Reporting;
using UnityEditor.UnityLinker;

namespace Purser.Editor
{
    /// <summary>
    /// Gives the package's <c>Runtime/link.xml</c> to the Unity linker when a player is built with managed code
    /// stripping: Unity collects <c>link.xml</c> files from the Assets folder only, not from packages. The file
    /// preserves the types Newtonsoft.Json creates by reflection and the Unity IAP internals the App Store promotional
    /// offer bridge calls.
    /// </summary>
    internal sealed class PurserLinkerProcessor : IUnityLinkerProcessor
    {
        /// <inheritdoc/>
        public int callbackOrder => 0;

        /// <summary>The full path of the package's link.xml; null when the package is not installed as a package (its
        /// link.xml is then under Assets, where Unity finds it by itself).</summary>
        public string GenerateAdditionalLinkXmlFile(BuildReport report, UnityLinkerBuildPipelineData data)
        {
            var package = UnityEditor.PackageManager.PackageInfo.FindForAssembly(typeof(BillingService).Assembly);
            if (package == null || string.IsNullOrEmpty(package.resolvedPath))
                return null;
            var path = Path.GetFullPath(Path.Combine(package.resolvedPath, "Runtime", "link.xml"));
            return File.Exists(path) ? path : null;
        }

        // Members of IUnityLinkerProcessor in older editors; nothing to do.

        /// <summary>Not used.</summary>
        public void OnBeforeRun(BuildReport report, UnityLinkerBuildPipelineData data)
        {
        }

        /// <summary>Not used.</summary>
        public void OnAfterRun(BuildReport report, UnityLinkerBuildPipelineData data)
        {
        }
    }
}
