using System;
using System.Net;

namespace Purser
{
    /// <summary>Which URLs the package talks to or opens: the billing server over https (plain http only on this
    /// machine, for development), store pages only over https.</summary>
    internal static class UrlPolicy
    {
        /// <summary>An absolute <c>https://</c> URL.</summary>
        public static bool IsHttps(string url) =>
            Uri.TryCreate(url, UriKind.Absolute, out var uri) && uri.Scheme == Uri.UriSchemeHttps && uri.Host.Length > 0;

        /// <summary>An https URL, or http to <c>localhost</c> / a loopback address (a local server in development).</summary>
        public static bool IsAllowedServerUrl(string url)
        {
            if (IsHttps(url))
                return true;
            if (!Uri.TryCreate(url, UriKind.Absolute, out var uri) || uri.Scheme != Uri.UriSchemeHttp)
                return false;
            var host = uri.Host.Trim('[', ']');
            return string.Equals(host, "localhost", StringComparison.OrdinalIgnoreCase) ||
                   (IPAddress.TryParse(host, out var ip) && IPAddress.IsLoopback(ip));
        }

        /// <summary>Why the billing server URL must not be used, or null. The Editor may use any URL.</summary>
        public static string ServerUrlProblem(string url, bool isEditor)
        {
            if (isEditor || string.IsNullOrWhiteSpace(url) || IsAllowedServerUrl(url))
                return null;
            return "The billing server URL must use https:// outside the Editor " +
                   "(plain http only for localhost): " + url;
        }
    }
}
