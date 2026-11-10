using System;
using System.Security.Cryptography;
using System.Text;

namespace Purser
{
    /// <summary>
    /// Binding of store purchases to a player. Must stay identical to
    /// <c>purser_core::google_account_id</c> / <c>apple_account_token</c> on the server.
    /// </summary>
    public static class AccountIds
    {
        /// <summary>Google <c>obfuscatedAccountId</c> (max 64 chars): the id itself, or SHA-256 hex if longer.</summary>
        public static string GoogleAccountId(string userId)
        {
            if (userId == null)
                throw new ArgumentNullException(nameof(userId));
            return userId.Length <= 64 ? userId : Sha256Hex(userId).Substring(0, 64);
        }

        /// <summary>Apple <c>appAccountToken</c>: UUID v8 from the first 16 bytes of SHA-256(userId).</summary>
        public static Guid AppleAccountToken(string userId)
        {
            if (userId == null)
                throw new ArgumentNullException(nameof(userId));
            byte[] digest;
            using (var sha = SHA256.Create())
                digest = sha.ComputeHash(Encoding.UTF8.GetBytes(userId));
            var b = new byte[16];
            Array.Copy(digest, b, 16);
            b[6] = (byte)((b[6] & 0x0f) | 0x80); // version 8
            b[8] = (byte)((b[8] & 0x3f) | 0x80); // RFC 4122 variant
            // Parse from text: Guid(byte[]) would reorder the first fields (little-endian).
            var h = ToHex(b);
            return Guid.Parse($"{h.Substring(0, 8)}-{h.Substring(8, 4)}-{h.Substring(12, 4)}-{h.Substring(16, 4)}-{h.Substring(20, 12)}");
        }

        private static string Sha256Hex(string s)
        {
            using (var sha = SHA256.Create())
                return ToHex(sha.ComputeHash(Encoding.UTF8.GetBytes(s)));
        }

        private static string ToHex(byte[] bytes)
        {
            var sb = new StringBuilder(bytes.Length * 2);
            foreach (var x in bytes)
                sb.Append(x.ToString("x2"));
            return sb.ToString();
        }
    }
}
