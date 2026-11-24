using System;
using System.Numerics;
using System.Security.Cryptography;
using System.Threading.Tasks;

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
        /// <summary>(p - 5) / 8: the exponent of the square root (RFC 8032, 5.1.3).</summary>
        private static readonly BigInteger RootExponent;
        private static readonly BigInteger Mask255 = (BigInteger.One << 255) - 1;
        private static readonly BigInteger TwoP = 2 * P;
        private static readonly Point BasePoint;
        private static readonly Point Identity = new Point(BigInteger.Zero, BigInteger.One, BigInteger.One, BigInteger.Zero);

        /// <summary>The key decoded last: an app verifies with one key.</summary>
        private static PublicKey s_LastKey;

        static Ed25519()
        {
            D = Mod(-121665 * Inverse(121666));
            SqrtMinusOne = BigInteger.ModPow(2, (P - 1) / 4, P);
            RootExponent = (P - 5) / 8;
            var y = Mod(4 * Inverse(5));
            var x = RecoverX(y, 0) ?? throw new InvalidOperationException("Ed25519 base point");
            BasePoint = new Point(x, y, BigInteger.One, Mod(x * y));
        }

        /// <summary>A public key decoded once, with the points a verification adds.</summary>
        internal sealed class PublicKey
        {
            internal readonly byte[] Bytes;
            /// <summary>-A.</summary>
            internal readonly Point Negated;
            /// <summary>B - A.</summary>
            internal readonly Point BaseMinusKey;

            internal PublicKey(byte[] bytes, Point a)
            {
                Bytes = (byte[])bytes.Clone();
                Negated = new Point(Mod(-a.X), a.Y, a.Z, Mod(-a.T));
                BaseMinusKey = Add(BasePoint, Negated);
            }
        }

        /// <summary>The decoded key; null when <paramref name="publicKey"/> is not 32 bytes that encode a point of the
        /// curve. Keeps the key decoded last, so verifying with the same key again does not decode it again.</summary>
        public static PublicKey DecodeKey(byte[] publicKey)
        {
            if (publicKey == null || publicKey.Length != PublicKeyLength)
                return null;
            var last = s_LastKey;
            if (last != null && SameBytes(last.Bytes, publicKey))
                return last;
            var a = Decode(publicKey);
            if (a == null)
                return null;
            var key = new PublicKey(publicKey, a.Value);
            s_LastKey = key;
            return key;
        }

        /// <summary>
        /// Whether <paramref name="signature"/> (64 bytes: R || S) is a valid Ed25519 signature of
        /// <paramref name="message"/> by <paramref name="publicKey"/> (32 bytes). Rejects a non-canonical S (S ≥ L),
        /// a public key or R that is not a point of the curve, and wrong lengths. Uses the cofactored equation
        /// [8][S]B = [8]R + [8][k]A.
        /// </summary>
        public static bool Verify(byte[] publicKey, byte[] message, byte[] signature) =>
            VerifyWith(DecodeKey(publicKey), message, signature);

        /// <summary><see cref="Verify(byte[], byte[], byte[])"/> on a thread of the pool (<paramref name="offThread"/>;
        /// never on WebGL, which has no threads), so a frame does not wait for it. The task has no continuation of its
        /// own: it may be waited for on any thread.</summary>
        public static Task<bool> VerifyAsync(byte[] publicKey, byte[] message, byte[] signature, bool offThread)
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return Task.FromResult(Verify(publicKey, message, signature));
#else
            return offThread
                ? Task.Run(() => Verify(publicKey, message, signature))
                : Task.FromResult(Verify(publicKey, message, signature));
#endif
        }

        /// <summary><see cref="Verify(byte[], byte[], byte[])"/> with a decoded key (<see cref="DecodeKey"/>); false for
        /// a null key.</summary>
        public static bool VerifyWith(PublicKey key, byte[] message, byte[] signature)
        {
            if (key == null || signature == null || signature.Length != SignatureLength || message == null)
                return false;

            var rBytes = new byte[32];
            var sBytes = new byte[32];
            Array.Copy(signature, 0, rBytes, 0, 32);
            Array.Copy(signature, 32, sBytes, 0, 32);
            if (FromLittleEndian(sBytes) >= L)
                return false;
            var r = Decode(rBytes);
            if (r == null)
                return false;

            byte[] digest;
            using (var sha = SHA512.Create())
            {
                var input = new byte[64 + message.Length];
                Array.Copy(rBytes, 0, input, 0, 32);
                Array.Copy(key.Bytes, 0, input, 32, 32);
                Array.Copy(message, 0, input, 64, message.Length);
                digest = sha.ComputeHash(input);
            }
            var k = (FromLittleEndian(digest) % L).ToByteArray();

            // [8][S]B = [8]R + [8][k]A, as [8]([S]B - [k]A) = [8]R: one pass over the bits of S and k.
            var q = DoubleScalar(sBytes, k, key);
            return AreEqual(Times8(q), Times8(r.Value));
        }

        // ---- field and curve arithmetic (extended coordinates: x = X/Z, y = Y/Z, x*y = T/Z) ----

        internal readonly struct Point
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

        /// <summary>x mod p in [0, p). 2^255 = 19 (mod p), so the bits above 255 fold back in multiplied by 19:
        /// cheaper than a division.</summary>
        private static BigInteger Mod(BigInteger x)
        {
            if (x.Sign < 0)
            {
                var r = x % P;
                return r.Sign < 0 ? r + P : r;
            }
            while (x > Mask255)
                x = (x & Mask255) + 19 * (x >> 255);
            return x >= P ? x - P : x;
        }

        private static BigInteger Inverse(BigInteger x) => BigInteger.ModPow(Mod(x), P - 2, P);

        private static Point Add(Point p, Point q)
        {
            var a = Mod((p.Y - p.X) * (q.Y - q.X));
            var b = Mod((p.Y + p.X) * (q.Y + q.X));
            var c = Mod(2 * p.T * q.T * D);
            var d = Mod(2 * p.Z * q.Z);
            // + p keeps the factors non-negative (the fast path of Mod).
            var e = b - a + P;
            var f = d - c + P;
            var g = d + c;
            var h = b + a;
            return new Point(Mod(e * f), Mod(g * h), Mod(f * g), Mod(e * h));
        }

        /// <summary>p + p with the doubling formulas (four squarings and four multiplications; <see cref="Add"/> takes
        /// nine multiplications).</summary>
        private static Point Double(Point p)
        {
            var a = Mod(p.X * p.X);
            var b = Mod(p.Y * p.Y);
            var c = Mod(2 * p.Z * p.Z);
            var sum = p.X + p.Y;
            // Multiples of p keep the factors non-negative (the fast path of Mod).
            var e = Mod(sum * sum) + TwoP - a - b;
            var g = b - a + P;
            var f = g - c + P;
            var h = TwoP - a - b;
            return new Point(Mod(e * f), Mod(g * h), Mod(f * g), Mod(e * h));
        }

        private static Point Times8(Point p) => Double(Double(Double(p)));

        /// <summary>[s]B - [k]A in one double-and-add pass (Shamir's trick): each step doubles once and adds B, -A or
        /// B - A according to the bits of s and k (little-endian bytes).</summary>
        private static Point DoubleScalar(byte[] s, byte[] k, PublicKey key)
        {
            var q = Identity;
            for (var i = Math.Max(s.Length, k.Length) * 8 - 1; i >= 0; i--)
            {
                q = Double(q);
                var sBit = Bit(s, i);
                var kBit = Bit(k, i);
                if (sBit && kBit)
                    q = Add(q, key.BaseMinusKey);
                else if (sBit)
                    q = Add(q, BasePoint);
                else if (kBit)
                    q = Add(q, key.Negated);
            }
            return q;
        }

        private static bool Bit(byte[] littleEndian, int i)
        {
            var index = i >> 3;
            return index < littleEndian.Length && ((littleEndian[index] >> (i & 7)) & 1) != 0;
        }

        private static bool AreEqual(Point p, Point q) =>
            Mod(p.X * q.Z - q.X * p.Z).IsZero && Mod(p.Y * q.Z - q.Y * p.Z).IsZero;

        /// <summary>The x coordinate for <paramref name="y"/> whose low bit is <paramref name="sign"/>; null when
        /// there is none (RFC 8032, 5.1.3: one exponentiation for the division and the square root together).</summary>
        private static BigInteger? RecoverX(BigInteger y, int sign)
        {
            if (y >= P)
                return null;
            // x^2 = u / v with u = y^2 - 1, v = d y^2 + 1.
            var yy = Mod(y * y);
            var u = Mod(yy - 1);
            var v = Mod(D * yy + 1);
            if (u.IsZero)
                return sign == 0 ? BigInteger.Zero : (BigInteger?)null;
            var v3 = Mod(v * v % P * v);
            var v7 = Mod(v3 * v3 % P * v);
            var x = Mod(u * v3 % P * BigInteger.ModPow(Mod(u * v7), RootExponent, P));
            var check = Mod(v * x % P * x);
            if (check != u)
            {
                if (check != Mod(-u))
                    return null;
                x = Mod(x * SqrtMinusOne);
            }
            if ((int)(x & 1) != sign)
                x = P - x;
            return x;
        }

        private static Point? Decode(byte[] encoded)
        {
            var y = FromLittleEndian(encoded);
            var sign = (int)(y >> 255);
            y &= Mask255;
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

        private static bool SameBytes(byte[] a, byte[] b)
        {
            if (a.Length != b.Length)
                return false;
            for (var i = 0; i < a.Length; i++)
                if (a[i] != b[i])
                    return false;
            return true;
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

        /// <summary><see cref="Problem"/>, computed on a thread of the pool when <paramref name="offThread"/> (never on
        /// WebGL). The task has no continuation of its own: it may be waited for on any thread.</summary>
        public static Task<string> ProblemAsync(string publicKeyBase64, byte[] body, string signatureBase64, bool offThread)
        {
#if UNITY_WEBGL && !UNITY_EDITOR
            return Task.FromResult(Problem(publicKeyBase64, body, signatureBase64));
#else
            return offThread
                ? Task.Run(() => Problem(publicKeyBase64, body, signatureBase64))
                : Task.FromResult(Problem(publicKeyBase64, body, signatureBase64));
#endif
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
