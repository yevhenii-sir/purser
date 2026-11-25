import { useMemo, useState } from "react"
import { CopyIcon, KeyRoundIcon, Trash2Icon, UserPlusIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { RoleAccess, RoleMatrix } from "@/components/role-matrix"
import { AppsPicker } from "@/components/apps-picker"
import { call, createIdentity, select } from "@/lib/api"
import { ago, errorText } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { useApps } from "@/lib/apps"
import { ROLES, roleLabel, useRole, type Role } from "@/lib/roles"
import type { PageProps } from "@/routes"

const hex = (id: unknown) => String(id).replace(/^0x/, "").toLowerCase()

interface AdminRow {
  identity: string
  protected: boolean
  role: string
  name: string
  added_by: string
  added_at_micros: number
  /** Comma-separated `game_key`s the admin is limited to; empty = every app. */
  game_keys: string
}

const keysOf = (a: AdminRow) => a.game_keys.split(",").map((k) => k.trim()).filter(Boolean)

const roleOf = (a: AdminRow): Role => (a.protected ? "owner" : ((ROLES.some((r) => r.id === a.role) ? a.role : "viewer") as Role))

export function AdminsPage({ me }: PageProps) {
  const myRole = useRole()
  const [adding, setAdding] = useState(false)
  const { data, error, loading, reload } = useAsync(() => select<AdminRow>({ table: "admin" }))
  /** Roles the signed-in admin may give or take away (only the owner manages admins). */
  const manageable = (r: Role) => r !== "owner" && (myRole === "owner" || r !== "admin")
  const names = new Map((data || []).map((a) => [hex(a.identity), a.name]))

  const [editingApps, setEditingApps] = useState<AdminRow | null>(null)
  const appName = useAppNames()
  const setRole = async (a: AdminRow, role: string) => {
    await tryAct(() => call("admin_upsert_admin", JSON.stringify({ identity: hex(a.identity), name: a.name, role, game_keys: keysOf(a) })), "Role changed")
    reload()
  }
  return (
    <>
      <PageHeader
        title="Admins"
        description="Who can open this admin page and what each may do. Give a coworker a token with a role, and limit it to some apps if needed; the server checks both on every request."
        actions={<Button onClick={() => setAdding(true)}><UserPlusIcon /> Add admin</Button>}
      />
      <Section>
        {error ? <ErrorBox error={error} /> : loading || !data ? <Loading /> : (
          <DataTable
            rows={[...data].sort((a, b) => Number(b.protected) - Number(a.protected) || a.name.localeCompare(b.name))}
            rowKey={(a) => hex(a.identity)}
            columns={[
              {
                title: "Admin",
                className: "whitespace-normal",
                cell: (a) => (
                  <div className="min-w-0 space-y-0.5">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-medium">{a.name || (a.protected ? "Owner" : "Unnamed")}</span>
                      {hex(a.identity) === me && <Tone tone="ok">you</Tone>}
                    </div>
                    <Mono className="break-all text-muted-foreground">{hex(a.identity)}</Mono>
                  </div>
                ),
              },
              {
                title: "Role",
                cell: (a) => {
                  const r = roleOf(a)
                  if (!manageable(r) || hex(a.identity) === me) return <Tone tone={r === "owner" ? "muted" : "ok"}>{roleLabel(r)}</Tone>
                  return (
                    <Select value={r} onValueChange={(v) => setRole(a, v)}>
                      <SelectTrigger className="h-8 w-32"><SelectValue /></SelectTrigger>
                      <SelectContent>{ROLES.filter((x) => manageable(x.id)).map((x) => <SelectItem key={x.id} value={x.id}>{x.label}</SelectItem>)}</SelectContent>
                    </Select>
                  )
                },
              },
              {
                title: "Apps",
                className: "whitespace-normal",
                cell: (a) => {
                  const keys = keysOf(a)
                  const text = a.protected || !keys.length ? "Every app" : keys.map(appName).join(", ")
                  if (a.protected || !manageable(roleOf(a)) || hex(a.identity) === me) return <span className="text-sm text-muted-foreground">{text}</span>
                  return (
                    <Button variant="ghost" size="xs" className="h-auto max-w-64 justify-start text-left whitespace-normal" title="Change the apps" onClick={() => setEditingApps(a)}>
                      {text}
                    </Button>
                  )
                },
              },
              {
                title: "Added",
                cell: (a) => (
                  <span className="text-xs text-muted-foreground">
                    {Number(a.added_at_micros) > 0 ? ago(a.added_at_micros) : "—"}
                    {a.added_by && <> by {names.get(hex(a.added_by)) || hex(a.added_by).slice(0, 10) + "…"}</>}
                  </span>
                ),
              },
              {
                title: "",
                className: "text-right",
                cell: (a) => manageable(roleOf(a)) && hex(a.identity) !== me && (
                  <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Remove" onClick={async () => {
                    if (!confirm(`Remove ${a.name || "this admin"}? Their token stops working here at once.`)) return
                    await tryAct(() => call("admin_remove_admin", hex(a.identity)), "Removed")
                    reload()
                  }}><Trash2Icon /></Button>
                ),
              },
            ]}
          />
        )}
      </Section>
      <Section title="What each role may do" description="The Owner is the identity that published the database. Limiting an admin to some apps narrows any role to those apps.">
        <RoleMatrix highlight={myRole} />
      </Section>
      <AddAdminDialog open={adding} onClose={() => setAdding(false)} onAdded={reload} roles={ROLES.filter((r) => manageable(r.id))} />
      <Dialog open={!!editingApps} onOpenChange={(o) => !o && setEditingApps(null)}>
        <DialogContent className="sm:max-w-md">
          {editingApps && <EditApps admin={editingApps} onClose={() => setEditingApps(null)} onSaved={reload} />}
        </DialogContent>
      </Dialog>
    </>
  )
}

/** Display names of app keys (the key itself for an app that no longer exists). */
function useAppNames() {
  const { groups } = useApps()
  const names = useMemo(() => new Map((groups || []).map((g) => [g.key, g.name])), [groups])
  return (key: string) => names.get(key) || key
}

/** The apps an admin is limited to: none ticked = every app (and the server-wide pages). */
function AdminAppsPicker({ value, onChange }: { value: string[]; onChange: (keys: string[]) => void }) {
  const summary = value.length
    ? `Limited to ${value.length} app${value.length === 1 ? "" : "s"}: no server settings, keys of developer accounts, alerts or admins.`
    : "Every app and the server settings. Tick apps to limit this admin to them."
  return <AppsPicker value={value} onChange={onChange} summary={summary} />
}

function EditApps({ admin, onClose, onSaved }: { admin: AdminRow; onClose: () => void; onSaved: () => void }) {
  const [keys, setKeys] = useState(keysOf(admin))
  const save = async () => {
    const ok = await tryAct(() => call("admin_upsert_admin", JSON.stringify({ identity: hex(admin.identity), name: admin.name, role: admin.role, game_keys: keys })), "Saved")
    if (ok !== undefined) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>Apps of {admin.name || "this admin"}</DialogTitle>
        <DialogDescription>Which apps this admin may see and change.</DialogDescription>
      </DialogHeader>
      <AdminAppsPicker value={keys} onChange={setKeys} />
      <DialogFooter><Button onClick={save}>Save</Button></DialogFooter>
    </>
  )
}

function AddAdminDialog({ open, onClose, onAdded, roles }: { open: boolean; onClose: () => void; onAdded: () => void; roles: typeof ROLES }) {
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-lg">{open && <AddAdmin onClose={onClose} onAdded={onAdded} roles={roles} />}</DialogContent>
    </Dialog>
  )
}

function AddAdmin({ onClose, onAdded, roles }: { onClose: () => void; onAdded: () => void; roles: typeof ROLES }) {
  const [name, setName] = useState("")
  const [role, setRole] = useState<string>(roles.find((r) => r.id === "viewer")?.id || roles[0]?.id || "viewer")
  const [identity, setIdentity] = useState("")
  const [apps, setApps] = useState<string[]>([])
  const [busy, setBusy] = useState(false)
  /** The generated token, shown once. */
  const [token, setToken] = useState<string | null>(null)

  const save = async (id: string) => {
    await call("admin_upsert_admin", JSON.stringify({ identity: hex(id), name: name.trim(), role, game_keys: apps }))
    onAdded()
  }

  const generate = async () => {
    if (!name.trim()) return toast.error("Enter a name")
    setBusy(true)
    try {
      const r = await createIdentity()
      await save(r.identity)
      setToken(r.token)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const addExisting = async () => {
    if (!/^(0x)?[0-9a-fA-F]{64}$/.test(identity.trim())) return toast.error("An identity is 64 hex characters")
    setBusy(true)
    const ok = await tryAct(async () => { await save(identity.trim()); return true }, "Added")
    setBusy(false)
    if (ok) onClose()
  }

  const link = location.origin + location.pathname
  if (token) {
    return (
      <>
        <DialogHeader>
          <DialogTitle>Token for {name.trim()}</DialogTitle>
          <DialogDescription>
            Send the token and the link to {name.trim()} privately. It is shown only now and cannot be recovered: if it is lost, remove this admin and add a new one.
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-3">
          <Field label="Admin page">
            <Mono className="break-all">{link}</Mono>
          </Field>
          <Field label="Token">
            <div className="flex gap-2">
              <Input readOnly className="font-mono text-xs" value={token} onFocus={(e) => e.target.select()} />
              <Button variant="outline" size="icon" aria-label="Copy the token" onClick={() => tryAct(() => navigator.clipboard.writeText(token), "Copied")}><CopyIcon /></Button>
            </div>
          </Field>
          <Button variant="outline" size="sm" onClick={() => tryAct(() => navigator.clipboard.writeText(`${link}\n${token}`), "Copied the link and the token")}>
            <CopyIcon /> Copy both
          </Button>
        </div>
        <DialogFooter><Button onClick={onClose}>Done</Button></DialogFooter>
      </>
    )
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>Add admin</DialogTitle>
        <DialogDescription>Generate a token to hand to a coworker, or add the identity of someone who already has a SpacetimeDB login.</DialogDescription>
      </DialogHeader>
      <DialogBody className="space-y-4">
        <Field label="Name"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Anna (support)" autoFocus /></Field>
        <Field label="Role">
          <Select value={role} onValueChange={setRole}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{roles.map((r) => <SelectItem key={r.id} value={r.id}>{r.label}</SelectItem>)}</SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">{roles.find((r) => r.id === role)?.description}</p>
        </Field>
        <Field label="Apps">
          <AdminAppsPicker value={apps} onChange={setApps} />
        </Field>
        <Field label="This admin will be able to">
          <RoleAccess role={role as Role} limited={apps.length > 0} />
        </Field>
        <Button className="w-full" onClick={generate} disabled={busy}><KeyRoundIcon /> Generate a token</Button>
        <div className="space-y-2 border-t pt-4">
          <p className="text-xs text-muted-foreground">Or add an existing identity (theirs: <code>spacetime login show --identity</code>):</p>
          <div className="flex gap-2">
            <Input className="font-mono text-xs" placeholder="identity hex (64 chars)" value={identity} onChange={(e) => setIdentity(e.target.value)} />
            <Button variant="outline" onClick={addExisting} disabled={busy || !identity.trim()}>Add</Button>
          </div>
        </div>
      </DialogBody>
    </>
  )
}
