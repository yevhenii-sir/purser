import { Fragment } from "react"
import { CheckIcon, MinusIcon } from "lucide-react"
import { ACTIONS, ALL_ROLES, can, roleLabel, type Role } from "@/lib/roles"
import { cn } from "@/lib/utils"

const EVERY_APP = "Not for admins limited to some apps"

/** What every role may do: actions by group, a column per role. `highlight`: a role to mark. */
export function RoleMatrix({ highlight }: { highlight?: Role }) {
  const groups = [...new Set(ACTIONS.map((a) => a.group))]
  return (
    <div className="overflow-x-auto">
      <table className="w-full min-w-[640px] text-sm">
        <thead>
          <tr className="border-b text-xs text-muted-foreground">
            <th className="py-2 pr-3 text-left font-medium">Action</th>
            {ALL_ROLES.map((r) => (
              <th key={r} className={cn("w-20 px-2 py-2 text-center font-medium", r === highlight && "bg-muted text-foreground")}>{roleLabel(r)}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {groups.map((g) => (
            <Fragment key={g}>
              <tr>
                <td colSpan={ALL_ROLES.length + 1} className="pt-3 pb-1 text-xs font-semibold tracking-wide text-muted-foreground uppercase">{g}</td>
              </tr>
              {ACTIONS.filter((a) => a.group === g).map((a) => (
                <tr key={a.action} className="border-b last:border-0">
                  <td className="py-1.5 pr-3">
                    {a.action}
                    {a.everyApp && <span className="ms-2 rounded border px-1 text-[10px] text-muted-foreground" title={EVERY_APP}>every app</span>}
                  </td>
                  {ALL_ROLES.map((r) => (
                    <td key={r} className={cn("px-2 py-1.5 text-center", r === highlight && "bg-muted")}>
                      {can(r, a.cap)
                        ? <CheckIcon className="mx-auto size-4 text-emerald-600 dark:text-emerald-400" aria-label="allowed" />
                        : <MinusIcon className="mx-auto size-4 text-muted-foreground/50" aria-label="not allowed" />}
                    </td>
                  ))}
                </tr>
              ))}
            </Fragment>
          ))}
        </tbody>
      </table>
      <p className="mt-2 text-xs text-muted-foreground">
        <span className="rounded border px-1 text-[10px]">every app</span> — {EVERY_APP.toLowerCase()}: such an admin sees and changes only their apps. The server checks the role and the apps on every request.
      </p>
    </div>
  )
}

/** What one role may do with the chosen apps, for the add-admin dialog. */
export function RoleAccess({ role, limited }: { role: Role; limited: boolean }) {
  return (
    <ul className="space-y-1 text-xs">
      {ACTIONS.map((a) => {
        const byRole = can(role, a.cap)
        const ok = byRole && !(limited && a.everyApp)
        return (
          <li key={a.action} className={cn("flex items-start gap-1.5", !ok && "text-muted-foreground")}>
            {ok ? <CheckIcon className="mt-0.5 size-3.5 shrink-0 text-emerald-600 dark:text-emerald-400" /> : <MinusIcon className="mt-0.5 size-3.5 shrink-0 opacity-50" />}
            <span className="min-w-0">
              {a.action}
              {byRole && !ok && <span className="ms-1">(not when limited to some apps)</span>}
            </span>
          </li>
        )
      })}
    </ul>
  )
}
