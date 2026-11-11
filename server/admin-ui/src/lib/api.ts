// The page is served by the billing module itself (GET /route/admin), so every API call is
// same-origin. Admin actions use the owner's SpacetimeDB token: admin reducers check the caller
// identity and private tables are readable only by the database owner. Secret values are never
// read back — only their names are listed.

/** The demo build (`npm run build:demo`): answers from a snapshot, no server, no sign-in. A build
 * constant, so the snapshot never reaches the page served by the module. */
export const DEMO = __DEMO__

export const DB_BASE = DEMO ? "/v1/database/billing-demo" : location.pathname.replace(/\/route\/admin\/?$/, "")
export const ROUTE = DB_BASE + "/route"
export const DB_NAME = decodeURIComponent(DB_BASE.split("/").pop() || "")
export const SERVED_BY_MODULE = DEMO || /\/v1\/database\/[^/]+\/route\/admin\/?$/.test(location.pathname)

const TOKEN_KEY = "purserAdminToken"

function storage(kind: "local" | "session"): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage
  } catch {
    return null
  }
}

export function getToken(): string {
  if (__DEMO__) return "demo"
  return storage("session")?.getItem(TOKEN_KEY) || storage("local")?.getItem(TOKEN_KEY) || ""
}

export function setToken(token: string, remember: boolean) {
  try {
    storage("session")?.setItem(TOKEN_KEY, token)
    if (remember) storage("local")?.setItem(TOKEN_KEY, token)
  } catch {
    // Private mode: the token lives in memory only for this page load.
  }
}

export function clearToken() {
  try {
    storage("session")?.removeItem(TOKEN_KEY)
    storage("local")?.removeItem(TOKEN_KEY)
  } catch {
    // ignore
  }
}

/** `hex_identity` claim of locally issued tokens; SpacetimeAuth tokens have none. */
export function tokenIdentity(token: string): string {
  try {
    const payload = JSON.parse(atob(token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/")))
    return String(payload.hex_identity || "")
  } catch {
    return ""
  }
}

export type Row = Record<string, any>

function unwrap(v: unknown): unknown {
  // Identity / Timestamp etc. come as single-element products: ["0x..."]
  if (Array.isArray(v) && v.length === 1 && typeof v[0] !== "object") return v[0]
  return v
}

/** Admins who are not the database owner cannot read private tables with /sql: their reads go
 * through the `admin_read` procedure, which checks their role. Set at sign-in (`admin_me`). */
let readThroughProcedure = false
export function setReadThroughProcedure(on: boolean) {
  readThroughProcedure = on
}

const SIMPLE_SELECT = /^\s*SELECT\s+(.+?)\s+FROM\s+(\w+)(?:\s+WHERE\s+(\w+)\s*=\s*'((?:[^']|'')*)')?\s*$/i

/** The page's `SELECT <cols> FROM <table> [WHERE <col> = '<value>']` through `admin_read`. */
async function readThroughAdminRead<T>(query: string): Promise<T[]> {
  const m = SIMPLE_SELECT.exec(query)
  if (!m) throw new Error("Only the database owner can run this query")
  const [, cols, table, column = "", raw = ""] = m
  const rows = await proc<Row[]>("admin_read", table, column, raw.replace(/''/g, "'"))
  const count = /^COUNT\(\*\)\s+AS\s+(\w+)$/i.exec(cols.trim())
  if (count) return [{ [count[1]]: rows.length }] as T[]
  if (cols.trim() === "*") return rows as T[]
  const names = cols.split(",").map((c) => c.trim())
  return rows.map((r) => Object.fromEntries(names.map((n) => [n, r[n]]))) as T[]
}

export async function sql<T = Row>(query: string): Promise<T[]> {
  if (__DEMO__) return (await import("./demo")).sql(query) as T[]
  if (readThroughProcedure) return readThroughAdminRead<T>(query)
  const res = await fetch(DB_BASE + "/sql", { method: "POST", headers: { Authorization: "Bearer " + getToken() }, body: query })
  const text = await res.text()
  if (!res.ok) throw new Error(text || "HTTP " + res.status)
  const result = JSON.parse(text)[0]
  // SpacetimeDB 2.10+ canonicalizes column names with digits (a_1b → a_1_b); map them back.
  const cols: string[] = result.schema.elements.map((e: any) => String(e.name.some ?? e.name).replace(/([a-z])_(\d)/g, "$1$2"))
  return result.rows.map((r: unknown[]) => Object.fromEntries(cols.map((c, i) => [c, unwrap(r[i])]))) as T[]
}

export const sqlStr = (v: unknown) => "'" + String(v).replace(/'/g, "''") + "'"

/** Rows of `table` whose `column` (an indexed one) equals one of `values`: one indexed query per
 * value, run in parallel, instead of reading the whole table. */
export async function sqlWhereIn<T = Row>(table: string, column: string, values: string[], columns = "*"): Promise<T[]> {
  const lists = await Promise.all([...new Set(values)].map((v) => sql<T>(`SELECT ${columns} FROM ${table} WHERE ${column} = ${sqlStr(v)}`)))
  return lists.flat()
}

/** Calls a reducer or procedure; returns the raw response text. */
export async function call(name: string, ...args: unknown[]): Promise<string> {
  if (__DEMO__) return (await import("./demo")).call(name, args)
  const res = await fetch(`${DB_BASE}/call/${name}`, {
    method: "POST",
    headers: { Authorization: "Bearer " + getToken(), "Content-Type": "application/json" },
    body: JSON.stringify(args),
  })
  const text = await res.text()
  if (!res.ok) throw new Error(text.replace(/^Error: /, "") || "HTTP " + res.status)
  return text
}

/** Admin procedures return a JSON document as a string: `{"error": "..."}` on failure. */
export async function proc<T = any>(name: string, ...args: unknown[]): Promise<T> {
  let v = JSON.parse(await call(name, ...args))
  if (typeof v === "string") v = JSON.parse(v)
  if (v && typeof v === "object" && "error" in v) throw new Error(String(v.error))
  return v as T
}

export async function route(path: string, body?: unknown): Promise<{ status: number; json: any }> {
  if (__DEMO__) return (await import("./demo")).route(path)
  const init: RequestInit = body === undefined ? {} : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }
  const res = await fetch(ROUTE + path, init)
  const text = await res.text()
  try {
    return { status: res.status, json: JSON.parse(text) }
  } catch {
    return { status: res.status, json: text }
  }
}

let versionPromise: Promise<string> | null = null

/** Version of the billing module (`/healthz`, the crate version); empty when the server does not say. */
export function moduleVersion(): Promise<string> {
  versionPromise ??= route("/healthz")
    .then((r) => (typeof r.json?.version === "string" ? r.json.version : ""))
    .catch(() => "")
  return versionPromise
}

/** Identity of the signed-in token: from the claims, otherwise asked from the server. */
export async function whoami(): Promise<string> {
  const fromClaims = tokenIdentity(getToken())
  if (fromClaims) return fromClaims.toLowerCase()
  try {
    return String(JSON.parse(await call("whoami"))).replace(/^0x/, "").toLowerCase()
  } catch {
    return ""
  }
}

/** A new SpacetimeDB identity and its token from this page's own server (`POST /v1/identity`). */
export async function createIdentity(): Promise<{ identity: string; token: string }> {
  if (__DEMO__) return (await import("./demo")).createIdentity()
  const res = await fetch(location.origin + "/v1/identity", { method: "POST" })
  if (!res.ok) {
    throw new Error(
      `This server does not give new identities to the admin page (POST /v1/identity: HTTP ${res.status}). ` +
        "Ask the coworker to sign in with the SpacetimeDB CLI (spacetime login) and send you the output of spacetime login show --identity, then add that identity.",
    )
  }
  const r = await res.json()
  return { identity: String(r.identity).replace(/^0x/, ""), token: String(r.token) }
}

export async function getSetting(key: string): Promise<string> {
  const rows = await sql<{ value: string }>(`SELECT value FROM server_state WHERE key = ${sqlStr(key)}`)
  return rows[0]?.value || ""
}

/** Public route base for store notification URLs and clients (the page may be opened through an SSH tunnel). */
export async function publicRoute(): Promise<string> {
  return (await getSetting("public_base_url")) || location.origin + ROUTE
}

export function genSecret(): string {
  const b = new Uint8Array(32)
  crypto.getRandomValues(b)
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("")
}
