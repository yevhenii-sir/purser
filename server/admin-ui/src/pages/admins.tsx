import { useState } from "react"
import { CopyIcon, KeyRoundIcon, Trash2Icon, UserPlusIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { call, createIdentity, select } from "@/lib/api"
import { ago, errorText } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
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
}

const roleOf = (a: AdminRow): Role => (a.protected ? "owner" : ((ROLES.some((r) => r.id === a.role) ? a.role : "viewer") as Role))

export function AdminsPage({ me }: PageProps) {
  const myRole = useRole()
  const [adding, setAdding] = useState(false)
  const { data, error, loading, reload } = useAsync(() => select<AdminRow>({ table: "admin" }))
  /** Roles the signed-in admin may give or take away (only the owner manages admins). */
  const manageable = (r: Role) => r !== "owner" && (myRole === "owner" || r !== "admin")
  const names = new Map((data || []).map((a) => [hex(a.identity), a.name]))

  const setRole = async (a: AdminRow, role: string) => {
    await tryAct(() => call("admin_upsert_admin", JSON.stringify({ identity: hex(a.identity), name: a.name, role })), "Role changed")
    reload()
  }
  return (
    <>
      <PageHeader
        title="Admins"
        description="Who can open this admin page and what each may do. Give a coworker a token with a role; the server checks the role on every request."
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
      <Section title="Roles">
        <ul className="space-y-1 text-sm">
          <li><b>Owner</b> <span className="text-muted-foreground">— the identity that published the database: everything, including delivery hosts, purchase import and admins with the Admin role.</span></li>
          {ROLES.map((r) => <li key={r.id}><b>{r.label}</b> <span className="text-muted-foreground">— {r.description}.</span></li>)}
        </ul>
      </Section>
      <AddAdminDialog open={adding} onClose={() => setAdding(false)} onAdded={reload} roles={ROLES.filter((r) => manageable(r.id))} />
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
  const [busy, setBusy] = useState(false)
  /** The generated token, shown once. */
  const [token, setToken] = useState<string | null>(null)

  const save = async (id: string) => {
    await call("admin_upsert_admin", JSON.stringify({ identity: hex(id), name: name.trim(), role }))
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
      <div className="space-y-4">
        <Field label="Name"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Anna (support)" autoFocus /></Field>
        <Field label="Role">
          <Select value={role} onValueChange={setRole}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{roles.map((r) => <SelectItem key={r.id} value={r.id}>{r.label}</SelectItem>)}</SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">{roles.find((r) => r.id === role)?.description}</p>
        </Field>
        <Button className="w-full" onClick={generate} disabled={busy}><KeyRoundIcon /> Generate a token</Button>
        <div className="space-y-2 border-t pt-4">
          <p className="text-xs text-muted-foreground">Or add an existing identity (theirs: <code>spacetime login show --identity</code>):</p>
          <div className="flex gap-2">
            <Input className="font-mono text-xs" placeholder="identity hex (64 chars)" value={identity} onChange={(e) => setIdentity(e.target.value)} />
            <Button variant="outline" onClick={addExisting} disabled={busy || !identity.trim()}>Add</Button>
          </div>
        </div>
      </div>
    </>
  )
}
