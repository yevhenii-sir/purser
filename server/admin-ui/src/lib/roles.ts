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

export const roleLabel = (r: Role) => (r === "owner" ? "Owner" : ROLES.find((x) => x.id === r)?.label || r)

export const RoleContext = createContext<Role>("owner")

export const useRole = () => useContext(RoleContext)

/** `can(cap)` for the signed-in admin. */
export function useCan() {
  const role = useContext(RoleContext)
  return (cap: Cap) => can(role, cap)
}
