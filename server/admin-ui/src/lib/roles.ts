import { createContext, useContext } from "react"

/** Admin roles, the same as `purser_core::roles` on the server (which enforces them). */
export type Role = "owner" | "admin" | "editor" | "support" | "viewer"

/** What an action needs (`purser_core::roles::Cap`). */
export type Cap = "read" | "support" | "configure" | "secrets" | "admins" | "owner"

const CAPS: Record<Cap, Role[]> = {
  read: ["owner", "admin", "editor", "support", "viewer"],
  support: ["owner", "admin", "support"],
  configure: ["owner", "admin", "editor"],
  secrets: ["owner", "admin"],
  admins: ["owner", "admin"],
  owner: ["owner"],
}

export const can = (role: Role, cap: Cap) => CAPS[cap].includes(role)

/** Roles an admin can be given, with what they may do. */
export const ROLES: { id: Exclude<Role, "owner">; label: string; description: string }[] = [
  { id: "admin", label: "Admin", description: "Everything, including keys and other admins (only the owner manages admins with this role)" },
  { id: "editor", label: "Editor", description: "Builds, products, paywalls, integrations, alerts and settings; no keys, no admins" },
  { id: "support", label: "Support", description: "Look up players, grant, revoke and retry purchases; read everything else" },
  { id: "viewer", label: "Viewer", description: "Read-only: revenue, purchases, players and the configuration" },
]

/** What each role may do, for the matrix on the Admins page: every admin action of the server, by
 * the capability it checks (`purser_core::roles::Cap`). `everyApp`: refused to admins limited to some
 * apps (the server's `access::require`). */
export const ACTIONS: { group: string; action: string; cap: Cap; everyApp?: boolean }[] = [
  { group: "See", action: "Revenue, purchases, players, apps, products, paywalls and settings", cap: "read" },
  { group: "See", action: "Which keys are set (never their values)", cap: "read" },
  { group: "Players", action: "Grant, revoke, retry and move purchases", cap: "support" },
  { group: "Players", action: "Export or erase a player's data", cap: "support" },
  { group: "Apps", action: "Builds, products, store catalogs, creating products in the stores", cap: "configure" },
  { group: "Apps", action: "App settings: name, child-directed, purchase transfers", cap: "configure" },
  { group: "Apps", action: "Paywalls, placements and A/B tests", cap: "configure" },
  { group: "Apps", action: "Integrations: set up, test, send events again", cap: "configure" },
  { group: "Keys", action: "Store and integration keys of a build", cap: "secrets" },
  { group: "Keys", action: "Server keys for a game's backend", cap: "secrets" },
  { group: "Keys", action: "Keys of developer accounts and default keys, Google Cloud setup", cap: "secrets", everyApp: true },
  { group: "Server", action: "Server settings, exchange rates, store fee, revenue rebuild, worker restart", cap: "configure", everyApp: true },
  { group: "Server", action: "Owner alerts: rules and channels", cap: "configure", everyApp: true },
  { group: "Admins", action: "Add, change and remove editors, support and viewers; the audit log", cap: "admins", everyApp: true },
  { group: "Owner", action: "Add, change and remove admins with the Admin role", cap: "owner" },
  { group: "Owner", action: "Delivery hosts, purchase import, ledger reindex", cap: "owner" },
]

/** Every role, the owner first (the matrix columns). */
export const ALL_ROLES: Role[] = ["owner", "admin", "editor", "support", "viewer"]

export const roleLabel = (r: Role) => (r === "owner" ? "Owner" : ROLES.find((x) => x.id === r)?.label || r)

export const RoleContext = createContext<Role>("owner")

/** The apps (`game_key`) the signed-in admin is limited to; empty = every app. */
export const ScopeContext = createContext<string[]>([])

/** The signed-in admin is limited to some apps: server-wide pages and actions are hidden. */
export const useScoped = () => useContext(ScopeContext).length > 0

export const useRole = () => useContext(RoleContext)

/** `can(cap)` for the signed-in admin. */
export function useCan() {
  const role = useContext(RoleContext)
  return (cap: Cap) => can(role, cap)
}
