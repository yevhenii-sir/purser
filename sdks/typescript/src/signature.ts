// Signed answers: `/entitlements` and `/restore` carry `X-Purser-Signature`, the base64 Ed25519
// signature of the exact body, when the build has a response signing key.

export const SIGNATURE_HEADER = "X-Purser-Signature";

function fromBase64(s: string): Uint8Array {
  const bin = atob(s.trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Why `body` cannot be trusted, or undefined when `signature` is a valid signature of it by `publicKey` (base64). */
export async function signatureProblem(publicKey: string, body: Uint8Array, signature: string | null): Promise<string | undefined> {
  if (!signature) return "the answer is not signed";
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) return "Web Crypto (crypto.subtle) is not available to check the signature";
  let key: Uint8Array;
  let sig: Uint8Array;
  try {
    key = fromBase64(publicKey);
    sig = fromBase64(signature);
  } catch {
    return "the signature or the public key is not base64";
  }
  if (key.length !== 32) return "the response signing public key must be 32 bytes (base64)";
  try {
    const k = await subtle.importKey("raw", key as BufferSource, { name: "Ed25519" }, false, ["verify"]);
    const ok = await subtle.verify({ name: "Ed25519" }, k, sig as BufferSource, body as BufferSource);
    return ok ? undefined : "the answer's signature is wrong";
  } catch (e) {
    return `the signature could not be checked (Ed25519 in Web Crypto): ${(e as Error).message}`;
  }
}
