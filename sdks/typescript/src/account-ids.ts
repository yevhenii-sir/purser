// Account binding ids the server checks consumables against (purser_core::google_account_id /
// apple_account_token): pass them to the store when the player buys.

function subtle(): SubtleCrypto {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new Error("Web Crypto (crypto.subtle) is not available");
  return s;
}

async function sha256(text: string): Promise<Uint8Array> {
  return new Uint8Array(await subtle().digest("SHA-256", new TextEncoder().encode(text) as BufferSource));
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Google `obfuscatedAccountId`: the player id, or the SHA-256 hex of an id longer than 64 characters. */
export async function googleAccountId(userId: string): Promise<string> {
  if ([...userId].length <= 64) return userId;
  return hex(await sha256(userId)).slice(0, 64);
}

/** Apple `appAccountToken`: a UUID (version 8) from the first 16 bytes of SHA-256 of the player id. */
export async function appleAccountToken(userId: string): Promise<string> {
  const b = (await sha256(userId)).slice(0, 16);
  b[6] = (b[6] & 0x0f) | 0x80;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = hex(b);
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20, 32)}`;
}
