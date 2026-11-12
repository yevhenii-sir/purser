// The page is served by the billing module itself (GET /route/admin), so every API call is
// same-origin. Admin actions use the owner's SpacetimeDB token: admin reducers check the caller
// identity and private tables are readable only by the database owner. Secret values are never
// read back — only their names are listed.

/** The demo build (`npm run build:demo`): answers from a snapshot, no server, no sign-in. A build
 * constant, so the snapshot never reaches the page served by the module. */
export const DEMO = __DEMO__

/** Name of the demo database in its URLs. Not in demo.ts: a static import of it would put the snapshot into the module page. */
const DEMO_DB = "billing-demo"

export const DB_BASE = DEMO ? `/v1/database/${DEMO_DB}` : location.pathname.replace(/\/route\/admin\/?$/, "")
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

/** The token when the browser refuses storage (private mode, blocked site data): kept for this page load only. */
let memoryToken = ""

export function getToken(): string {
  if (__DEMO__) return "demo"
  try {
    return storage("session")?.getItem(TOKEN_KEY) || storage("local")?.getItem(TOKEN_KEY) || memoryToken
  } catch {
    return memoryToken
  }
}

export function setToken(token: string, remember: boolean) {
  memoryToken = token
  try {
    storage("session")?.setItem(TOKEN_KEY, token)
    if (remember) storage("local")?.setItem(TOKEN_KEY, token)
  } catch {
    // Storage refused or full: `memoryToken` holds it.
  }
}

export function clearToken() {
  memoryToken = ""
  try {
    storage("session")?.removeItem(TOKEN_KEY)
    storage("local")?.removeItem(TOKEN_KEY)
  } catch {
    // Nothing could be stored either.
  }
}

/** `hex_identity` claim of locally issued tokens; SpacetimeAuth tokens have none. */
function tokenIdentity(token: string): string {
  try {
    const payload = JSON.parse(atob(token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/")))
    return String(payload.hex_identity || "")
  } catch {
    return ""
  }
}

/** A table row as `/sql` and `admin_read` return it. */
export type Row = Record<string, unknown>

/** A failed HTTP call; `status` tells a rejected token (401 / 403) from an unreachable or failing server. */
export class HttpError extends Error {
  readonly status: number
  constructor(status: number, text: string) {
    super(text || `HTTP ${status}`)
    this.status = status
  }
}

/** The token was rejected (as opposed to the network or the server failing). */
export function isAuthError(e: unknown): boolean {
  if (e instanceof HttpError && (e.status === 401 || e.status === 403)) return true
  return (e instanceof Error ? e.message : String(e)).includes("[ERR_ADMIN_UNAUTHORIZED]")
}

function unwrap(v: unknown): unknown {
  // Identity / Timestamp etc. come as single-element products: ["0x..."]
  if (Array.isArray(v) && v.length === 1 && typeof v[0] !== "object") return v[0]
  return v
}

/** A read of one table: `/sql` for the owner, the `admin_read` procedure for other admins. */
export interface Query {
  table: string
  /** Columns to return; every column when absent. */
  columns?: readonly string[]
  /** Only rows whose `column` equals `equals` (or one of them): one indexed look-up per value. */
  where?: { column: string; equals: string | readonly string[] }
}

/** Admins who are not the database owner cannot read private tables with /sql: their reads go
 * through the `admin_read` procedure, which checks their role. Set at sign-in (`admin_me`). */
let readThroughProcedure = false
export function setReadThroughProcedure(on: boolean) {
  readThroughProcedure = on
}

const sqlStr = (v: string) => "'" + v.replace(/'/g, "''") + "'"

/** The (column, value) filters of a query; one without a filter is ("", ""), as `admin_read` takes it. */
export function lookups(q: Query): [column: string, value: string][] {
  if (!q.where) return [["", ""]]
  const values = typeof q.where.equals === "string" ? [q.where.equals] : [...new Set(q.where.equals)]
  return values.map((v) => [q.where!.column, v])
}

/** Only the `columns` of each row (all of them when absent). */
export function project(rows: Row[], columns?: readonly string[]): Row[] {
  return columns?.length ? rows.map((r) => Object.fromEntries(columns.map((c) => [c, r[c]]))) : rows
}

async function lookup(q: Query, column: string, value: string): Promise<Row[]> {
  if (readThroughProcedure) return project(await proc<Row[]>("admin_read", q.table, column, value), q.columns)
  const cols = q.columns?.length ? q.columns.join(", ") : "*"
  return sql(`SELECT ${cols} FROM ${q.table}${column ? ` WHERE ${column} = ${sqlStr(value)}` : ""}`)
}

/** Rows of a table, as the signed-in admin may read them. */
export async function select<T = Row>(q: Query): Promise<T[]> {
  if (__DEMO__) return (await import("./demo")).select(q) as T[]
  const lists = await Promise.all(lookups(q).map(([column, value]) => lookup(q, column, value)))
  return lists.flat() as T[]
}

interface SqlResult {
  schema: { elements: { name: { some: string } | string }[] }
  rows: unknown[][]
}

/** Raw `/sql`: only the database owner may run it. Reads go through `select`. */
export async function sql<T = Row>(query: string): Promise<T[]> {
  if (__DEMO__) return (await import("./demo")).sql()
  const res = await fetch(DB_BASE + "/sql", { method: "POST", headers: { Authorization: "Bearer " + getToken() }, body: query })
  const text = await res.text()
  if (!res.ok) throw new HttpError(res.status, text)
  const result = (JSON.parse(text) as SqlResult[])[0]
  // SpacetimeDB 2.10+ canonicalizes column names with digits (a_1b → a_1_b); map them back.
  const cols = result.schema.elements.map((e) => (typeof e.name === "string" ? e.name : e.name.some).replace(/([a-z])_(\d)/g, "$1$2"))
  return result.rows.map((r) => Object.fromEntries(cols.map((c, i) => [c, unwrap(r[i])]))) as T[]
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
  if (!res.ok) throw new HttpError(res.status, text.replace(/^Error: /, ""))
  return text
}

/** Admin procedures return a JSON document as a string: `{"error": "..."}` on failure. */
export async function proc<T = unknown>(name: string, ...args: unknown[]): Promise<T> {
  let v: unknown = JSON.parse(await call(name, ...args))
  if (typeof v === "string") v = JSON.parse(v)
  if (v && typeof v === "object" && "error" in v) throw new Error(String(v.error))
  return v as T
}

/** A client route of the module (`/route/<path>`): its status and the JSON (or text) it answered. */
export async function route<T = unknown>(path: string, body?: unknown): Promise<{ status: number; json: T }> {
  if (__DEMO__) return (await import("./demo")).route(path) as { status: number; json: T }
  const init: RequestInit = body === undefined ? {} : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }
  const res = await fetch(ROUTE + path, init)
  const text = await res.text()
  try {
    return { status: res.status, json: JSON.parse(text) }
  } catch {
    return { status: res.status, json: text as T }
  }
}

let versionPromise: Promise<string> | null = null

/** Version of the billing module (`/healthz`, the crate version); empty when the server does not say. */
export function moduleVersion(): Promise<string> {
  versionPromise ??= route<{ version?: unknown } | null>("/healthz")
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
  const rows = await select<{ value: string }>({ table: "server_state", columns: ["value"], where: { column: "key", equals: key } })
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
