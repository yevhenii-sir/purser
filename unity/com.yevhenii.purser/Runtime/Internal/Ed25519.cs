using System;
using System.Numerics;
using System.Security.Cryptography;

namespace Purser
{
    /// <summary>
    /// Ed25519 signature verification (RFC 8032, section 5.1.7) on <see cref="BigInteger"/>: no native code and no
    /// package dependency. Verification only (public data), so it does not need to run in constant time. Used for
    /// signed server responses (<see cref="BillingConfig.responseSigningPublicKey"/>).
    /// </summary>
    internal static class Ed25519
    {
        public const int PublicKeyLength = 32;
        public const int SignatureLength = 64;

        private static readonly BigInteger P = BigInteger.Pow(2, 255) - 19;
        /// <summary>Order of the base point (L in RFC 8032).</summary>
        private static readonly BigInteger L = BigInteger.Pow(2, 252) + BigInteger.Parse("27742317777372353535851937790883648493");
        private static readonly BigInteger D;
        private static readonly BigInteger SqrtMinusOne;
        private static readonly Point BasePoint;

        static Ed25519()
        {
            D = Mod(-121665 * Inverse(121666));
            SqrtMinusOne = BigInteger.ModPow(2, (P - 1) / 4, P);
            var y = Mod(4 * Inverse(5));
            var x = RecoverX(y, 0) ?? throw new InvalidOperationException("Ed25519 base point");
            BasePoint = new Point(x, y, BigInteger.One, Mod(x * y));
        }

        /// <summary>
        /// Whether <paramref name="signature"/> (64 bytes: R || S) is a valid Ed25519 signature of
        /// <paramref name="message"/> by <paramref name="publicKey"/> (32 bytes). Rejects a non-canonical S (S ≥ L),
        /// a public key or R that is not a point of the curve, and wrong lengths. Uses the cofactored equation
        /// [8][S]B = [8]R + [8][k]A.
        /// </summary>
        public static bool Verify(byte[] publicKey, byte[] message, byte[] signature)
        {
            if (publicKey == null || publicKey.Length != PublicKeyLength || signature == null ||
                signature.Length != SignatureLength || message == null)
                return false;

            var rBytes = new byte[32];
            var sBytes = new byte[32];
            Array.Copy(signature, 0, rBytes, 0, 32);
            Array.Copy(signature, 32, sBytes, 0, 32);
            var s = FromLittleEndian(sBytes);
            if (s >= L)
                return false;
            var a = Decode(publicKey);
            var r = Decode(rBytes);
            if (a == null || r == null)
                return false;

            byte[] digest;
            using (var sha = SHA512.Create())
            {
                var input = new byte[64 + message.Length];
                Array.Copy(rBytes, 0, input, 0, 32);
                Array.Copy(publicKey, 0, input, 32, 32);
                Array.Copy(message, 0, input, 64, message.Length);
                digest = sha.ComputeHash(input);
            }
            var k = FromLittleEndian(digest) % L;

            var left = Multiply(8 * s, BasePoint);
            var right = Multiply(8, Add(r.Value, Multiply(k, a.Value)));
            return AreEqual(left, right);
        }

        // ---- field and curve arithmetic (extended coordinates: x = X/Z, y = Y/Z, x*y = T/Z) ----

        private readonly struct Point
        {
            public readonly BigInteger X, Y, Z, T;

            public Point(BigInteger x, BigInteger y, BigInteger z, BigInteger t)
            {
                X = x;
                Y = y;
                Z = z;
                T = t;
            }
        }

        private static BigInteger Mod(BigInteger x)
        {
            var r = x % P;
            return r.Sign < 0 ? r + P : r;
        }

        private static BigInteger Inverse(BigInteger x) => BigInteger.ModPow(Mod(x), P - 2, P);

        private static Point Add(Point p, Point q)
        {
            var a = Mod((p.Y - p.X) * (q.Y - q.X));
            var b = Mod((p.Y + p.X) * (q.Y + q.X));
            var c = Mod(2 * p.T * q.T * D);
            var d = Mod(2 * p.Z * q.Z);
            var e = b - a;
            var f = d - c;
            var g = d + c;
            var h = b + a;
            return new Point(Mod(e * f), Mod(g * h), Mod(f * g), Mod(e * h));
        }

        private static Point Multiply(BigInteger scalar, Point point)
        {
            var result = new Point(BigInteger.Zero, BigInteger.One, BigInteger.One, BigInteger.Zero);
            while (scalar.Sign > 0)
            {
                if (!scalar.IsEven)
                    result = Add(result, point);
                point = Add(point, point);
                scalar >>= 1;
            }
            return result;
        }

        private static bool AreEqual(Point p, Point q) =>
            Mod(p.X * q.Z - q.X * p.Z).IsZero && Mod(p.Y * q.Z - q.Y * p.Z).IsZero;

        /// <summary>The x coordinate for <paramref name="y"/> whose low bit is <paramref name="sign"/>; null when
        /// there is none (RFC 8032, 5.1.3).</summary>
        private static BigInteger? RecoverX(BigInteger y, int sign)
        {
            if (y >= P)
                return null;
            var x2 = Mod((y * y - 1) * Inverse(D * y * y + 1));
            if (x2.IsZero)
                return sign == 0 ? BigInteger.Zero : (BigInteger?)null;
            var x = BigInteger.ModPow(x2, (P + 3) / 8, P);
            if (!Mod(x * x - x2).IsZero)
                x = Mod(x * SqrtMinusOne);
            if (!Mod(x * x - x2).IsZero)
                return null;
            if ((int)(x & 1) != sign)
                x = P - x;
            return x;
        }

        private static Point? Decode(byte[] encoded)
        {
            var y = FromLittleEndian(encoded);
            var sign = (int)(y >> 255);
            y &= (BigInteger.One << 255) - 1;
            var x = RecoverX(y, sign);
            if (x == null)
                return null;
            return new Point(x.Value, y, BigInteger.One, Mod(x.Value * y));
        }

        /// <summary>Unsigned little-endian integer.</summary>
        private static BigInteger FromLittleEndian(byte[] bytes)
        {
            var unsigned = new byte[bytes.Length + 1];
            Array.Copy(bytes, unsigned, bytes.Length);
            return new BigInteger(unsigned);
        }
    }

    /// <summary>Checks <c>X-Purser-Signature</c> of a server answer (<see cref="BillingConfig.responseSigningPublicKey"/>).</summary>
    internal static class ResponseSignature
    {
        public const string Header = "X-Purser-Signature";

        /// <summary>Null when <paramref name="signatureBase64"/> is a valid signature of <paramref name="body"/> by the
        /// key; otherwise why not.</summary>
        public static string Problem(string publicKeyBase64, byte[] body, string signatureBase64)
        {
            var key = DecodeKey(publicKeyBase64);
            if (key == null)
                return "responseSigningPublicKey is not a base64 Ed25519 public key (32 bytes)";
            if (string.IsNullOrEmpty(signatureBase64))
                return "the answer has no " + Header + " header";
            byte[] signature;
            try
            {
                signature = Convert.FromBase64String(signatureBase64.Trim());
            }
            catch (FormatException)
            {
                return Header + " is not base64";
            }
            return Ed25519.Verify(key, body ?? new byte[0], signature) ? null : "the response signature is invalid";
        }

        /// <summary>The 32-byte key, or null when <paramref name="base64"/> is not one.</summary>
        public static byte[] DecodeKey(string base64)
        {
            if (string.IsNullOrWhiteSpace(base64))
                return null;
            try
            {
                var key = Convert.FromBase64String(base64.Trim());
                return key.Length == Ed25519.PublicKeyLength ? key : null;
            }
            catch (FormatException)
            {
                return null;
            }
        }
    }
}
