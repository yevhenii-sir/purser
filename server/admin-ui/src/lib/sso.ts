// Signing in through an OpenID Connect provider (SpacetimeAuth on maincloud, any other provider on a
// self-hosted server): the authorization code flow with PKCE, done by the page itself. The provider's
// ID token is then used as the SpacetimeDB token: SpacetimeDB derives the identity from its issuer and
// subject, and the module checks that it was issued for this server's client.

import { route } from "@/lib/api"

/** The provider the server signs in with (`GET /admin/sign-in`); absent fields: tokens only. */
export interface SignInConfig {
  issuer: string
  client_id: string
  authorization_endpoint: string
  token_endpoint: string
}

export const SPACETIMEAUTH_ISSUER = "https://auth.spacetimedb.com/oidc"

/** What the page keeps between leaving for the provider and coming back (this tab only). */
const PENDING_KEY = "purserSignIn"

interface Pending {
  state: string
  nonce: string
  verifier: string
  token_endpoint: string
  client_id: string
  /** The page (`#route`) to return to. */
  hash: string
}

export async function signInConfig(): Promise<SignInConfig | null> {
  try {
    const r = await route<Partial<SignInConfig>>("/admin/sign-in")
    const c = r.json
    return r.status === 200 && c?.issuer && c.client_id && c.authorization_endpoint && c.token_endpoint ? (c as SignInConfig) : null
  } catch {
    return null
  }
}

/** The provider's name on the sign-in button. */
export function providerName(c: SignInConfig): string {
  if (c.issuer === SPACETIMEAUTH_ISSUER) return "SpacetimeAuth"
  try {
    return new URL(c.issuer).host
  } catch {
    return c.issuer
  }
}

/** Where the provider sends the browser back: this page without query or fragment. Register it at the provider. */
export function redirectUri(): string {
  return location.origin + location.pathname
}

function base64url(bytes: Uint8Array): string {
  let s = ""
  for (const b of bytes) s += String.fromCharCode(b)
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
}

const random = () => base64url(crypto.getRandomValues(new Uint8Array(32)))

async function challengeOf(verifier: string): Promise<string> {
  return base64url(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))))
}

/** Leaves for the provider's sign-in page. */
export async function startSignIn(c: SignInConfig): Promise<void> {
  const pending: Pending = { state: random(), nonce: random(), verifier: random(), token_endpoint: c.token_endpoint, client_id: c.client_id, hash: location.hash }
  sessionStorage.setItem(PENDING_KEY, JSON.stringify(pending))
  const url = new URL(c.authorization_endpoint)
  const params: Record<string, string> = {
    response_type: "code",
    client_id: c.client_id,
    redirect_uri: redirectUri(),
    scope: "openid profile email",
    state: pending.state,
    nonce: pending.nonce,
    code_challenge: await challengeOf(pending.verifier),
    code_challenge_method: "S256",
  }
  for (const [k, v] of Object.entries(params)) url.searchParams.set(k, v)
  location.assign(url.toString())
}

/** Claims of a JWT, unverified (the server checks the token; the page only reads what it says). */
export function claimsOf(token: string): Record<string, unknown> {
  try {
    return JSON.parse(atob(token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/")))
  } catch {
    return {}
  }
}

/**
 * The provider sent the browser back (`?code=…&state=…` or `?error=…`): trades the code for the ID
 * token. `null` when this page load is not such a return. The query is removed from the address.
 */
export async function finishSignIn(): Promise<string | null> {
  const q = new URLSearchParams(location.search)
  if (!q.has("code") && !q.has("error")) return null
  let pending: Pending | null = null
  try {
    pending = JSON.parse(sessionStorage.getItem(PENDING_KEY) || "null")
  } catch {
    pending = null
  }
  sessionStorage.removeItem(PENDING_KEY)
  history.replaceState(null, "", location.pathname + (pending?.hash || ""))
  if (q.has("error")) throw new Error(q.get("error_description") || q.get("error") || "the provider refused the sign-in")
  if (!pending || q.get("state") !== pending.state) throw new Error("the sign-in was not started from this tab: try again")
  const res = await fetch(pending.token_endpoint, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      code: q.get("code") || "",
      redirect_uri: redirectUri(),
      client_id: pending.client_id,
      code_verifier: pending.verifier,
    }),
  })
  const text = await res.text()
  let body: { id_token?: string; error?: string; error_description?: string } = {}
  try {
    body = JSON.parse(text)
  } catch {
    // Not JSON: reported below.
  }
  if (!res.ok || !body.id_token) throw new Error(body.error_description || body.error || `the provider answered HTTP ${res.status}`)
  if (claimsOf(body.id_token).nonce !== pending.nonce) throw new Error("the provider's token is not for this sign-in: try again")
  return body.id_token
}
