import { useMemo, useState } from "react"
import { CopyIcon, GlobeIcon, KeyRoundIcon, MailIcon, PencilIcon, Trash2Icon, UserPlusIcon } from "lucide-react"
import { toast } from "sonner"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { RoleAccess, RoleMatrix } from "@/components/role-matrix"
import { AppsPicker } from "@/components/apps-picker"
import { call, createIdentity, proc, select } from "@/lib/api"
import { providerName, redirectUri, signInConfig, SPACETIMEAUTH_ISSUER, type SignInConfig } from "@/lib/sso"
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

const keysOf = (a: { game_keys: string }) => a.game_keys.split(",").map((k) => k.trim()).filter(Boolean)

/** An invitation by email (`admin_invite`), claimed by the first sign-in through the provider. */
interface InviteRow {
  email: string
  name: string
  role: string
  game_keys: string
  invited_by: string
  invited_at_micros: number
  expires_at_micros: number
}

/** How an admin who joined through an invitation signs in (`admin_sign_in`). */
interface SignInRow {
  identity: string
  email: string
  /** The domain rule the admin joined through; empty: an invitation. */
  domain_rule: string
}

/** Everyone with a verified email of `domain` joins with `role` (`admin_domain_rule`). */
interface DomainRuleRow {
  domain: string
  role: string
  game_keys: string
  require_hosted_domain: boolean
  /** Admins who joined through it. */
  joined: number
  /** Emails removed from the admins: the rule no longer lets them in. */
  blocked: string[]
}

/** Roles a domain rule may give (`purser_core::sign_in::RULE_ROLES`). */
const RULE_ROLES = ROLES.filter((r) => r.id === "viewer" || r.id === "support")

const roleOf = (a: AdminRow): Role => (a.protected ? "owner" : ((ROLES.some((r) => r.id === a.role) ? a.role : "viewer") as Role))

const inviteRole = (i: InviteRow): Role => (ROLES.some((r) => r.id === i.role) ? i.role : "viewer") as Role

export function AdminsPage({ me }: PageProps) {
  const myRole = useRole()
  const [adding, setAdding] = useState(false)
  const { data, error, loading, reload: reloadAdmins } = useAsync(() => select<AdminRow>({ table: "admin" }))
  const invites = useAsync(() => select<InviteRow>({ table: "admin_invite" }))
  const bound = useAsync(() => select<SignInRow>({ table: "admin_sign_in" }))
  const domainRules = useAsync(() => select<DomainRuleRow>({ table: "admin_domain_rule" }))
  /** The rule being added (`null`: a new one) or changed; `undefined`: none. */
  const [editingRule, setEditingRule] = useState<DomainRuleRow | null | undefined>(undefined)
  const provider = useAsync(signInConfig)
  const reload = () => {
    reloadAdmins()
    invites.reload()
    bound.reload()
    domainRules.reload()
  }
  const emails = new Map((bound.data || []).map((b) => [hex(b.identity), b.email]))
  const viaRule = new Map((bound.data || []).filter((b) => b.domain_rule).map((b) => [hex(b.identity), b.domain_rule]))
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
                      {viaRule.get(hex(a.identity)) && <Tone tone="muted">via @{viaRule.get(hex(a.identity))}</Tone>}
                    </div>
                    {emails.get(hex(a.identity)) && <div className="text-xs text-muted-foreground">{emails.get(hex(a.identity))}</div>}
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
      {(invites.data?.length ?? 0) > 0 && (
        <Section title="Invitations" description="Each is claimed by the first sign-in through the provider with that verified email, until it expires.">
          <DataTable
            rows={invites.data || []}
            rowKey={(i) => i.email}
            columns={[
              { title: "Email", cell: (i) => <div className="min-w-0"><div className="font-medium">{i.email}</div>{i.name && <div className="text-xs text-muted-foreground">{i.name}</div>}</div> },
              { title: "Role", cell: (i) => <Tone tone="muted">{roleLabel(inviteRole(i))}</Tone> },
              { title: "Apps", className: "whitespace-normal", cell: (i) => <span className="text-sm text-muted-foreground">{keysOf(i).length ? keysOf(i).map(appName).join(", ") : "Every app"}</span> },
              {
                title: "Expires",
                cell: (i) => Number(i.expires_at_micros) / 1000 < Date.now()
                  ? <Tone tone="warn">expired</Tone>
                  : <span className="text-xs text-muted-foreground">{new Date(Number(i.expires_at_micros) / 1000).toLocaleDateString()}</span>,
              },
              {
                title: "",
                className: "text-right",
                cell: (i) => manageable(inviteRole(i)) && (
                  <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Cancel the invitation" onClick={async () => {
                    await tryAct(() => call("admin_cancel_invite", i.email), "Invitation canceled")
                    reload()
                  }}><Trash2Icon /></Button>
                ),
              },
            ]}
          />
        </Section>
      )}
      {(provider.data || (domainRules.data?.length ?? 0) > 0) && (
        <Section
          title="Domain rules"
          description="Everyone with a verified email of a company domain joins on their first sign-in, with a low role. Use it only with a provider the company controls."
          actions={provider.data && <Button variant="outline" size="sm" onClick={() => setEditingRule(null)}><GlobeIcon /> Add a rule</Button>}
        >
          {domainRules.error ? <ErrorBox error={domainRules.error} /> : !domainRules.data?.length ? (
            <p className="text-sm text-muted-foreground">None: only invited people and tokens get in.</p>
          ) : (
            <DataTable
              rows={domainRules.data}
              rowKey={(r) => r.domain}
              columns={[
                { title: "Domain", cell: (r) => <Mono>@{r.domain}</Mono> },
                { title: "Role", cell: (r) => <Tone tone="muted">{roleLabel(r.role as Role)}</Tone> },
                { title: "Apps", className: "whitespace-normal", cell: (r) => <span className="text-sm text-muted-foreground">{keysOf(r).length ? keysOf(r).map(appName).join(", ") : "Every app"}</span> },
                { title: "Organization check", cell: (r) => (r.require_hosted_domain ? <Tone tone="ok">hd claim</Tone> : <span className="text-xs text-muted-foreground">email only</span>) },
                {
                  title: "Joined",
                  className: "whitespace-normal",
                  cell: (r) => (
                    <div className="text-xs text-muted-foreground">
                      {r.joined}
                      {r.blocked?.length > 0 && <div title={r.blocked.join(", ")}>{r.blocked.length} removed, kept out</div>}
                    </div>
                  ),
                },
                {
                  title: "",
                  className: "text-right",
                  cell: (r) => (
                    <div className="flex justify-end gap-1">
                      <Button variant="ghost" size="icon-sm" aria-label="Change the rule" onClick={() => setEditingRule(r)}><PencilIcon /></Button>
                      <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Remove the rule" onClick={async () => {
                        const who = r.joined ? ` and the ${r.joined} admin${r.joined === 1 ? "" : "s"} who joined through it` : ""
                        if (!confirm(`Remove the rule for @${r.domain}${who}?`)) return
                        await tryAct(() => call("admin_delete_domain_rule", r.domain), "Rule removed")
                        reload()
                      }}><Trash2Icon /></Button>
                    </div>
                  ),
                },
              ]}
            />
          )}
        </Section>
      )}
      <Dialog open={editingRule !== undefined} onOpenChange={(o) => !o && setEditingRule(undefined)}>
        <DialogContent className="sm:max-w-lg">
          {editingRule !== undefined && provider.data && (
            <DomainRuleForm rule={editingRule} provider={provider.data} onClose={() => setEditingRule(undefined)} onSaved={reload} />
          )}
        </DialogContent>
      </Dialog>
      <SignInSection config={provider.data ?? null} loading={provider.loading} canEdit={myRole === "owner" || myRole === "admin"} onSaved={provider.reload} />
      <Section title="What each role may do" description="The Owner is the identity that published the database. Limiting an admin to some apps narrows any role to those apps.">
        <RoleMatrix highlight={myRole} />
      </Section>
      <AddAdminDialog open={adding} onClose={() => setAdding(false)} onAdded={reload} roles={ROLES.filter((r) => manageable(r.id))} provider={provider.data ?? null} />
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

function AddAdminDialog({ open, onClose, onAdded, roles, provider }: { open: boolean; onClose: () => void; onAdded: () => void; roles: typeof ROLES; provider: SignInConfig | null }) {
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-lg">{open && <AddAdmin onClose={onClose} onAdded={onAdded} roles={roles} provider={provider} />}</DialogContent>
    </Dialog>
  )
}

/** The provider admins sign in with: SpacetimeAuth on maincloud or any OpenID Connect provider. */
function SignInSection({ config, loading, canEdit, onSaved }: { config: SignInConfig | null; loading: boolean; canEdit: boolean; onSaved: () => void }) {
  const [issuer, setIssuer] = useState("")
  const [clientId, setClientId] = useState("")
  const [editing, setEditing] = useState(false)
  const [busy, setBusy] = useState(false)
  const edit = () => {
    setIssuer(config?.issuer || SPACETIMEAUTH_ISSUER)
    setClientId(config?.client_id || "")
    setEditing(true)
  }
  const save = async (i: string, c: string, ok: string) => {
    setBusy(true)
    const done = await tryAct(() => proc("admin_set_sign_in", i.trim(), c.trim()), ok)
    setBusy(false)
    if (done !== undefined) {
      setEditing(false)
      onSaved()
    }
  }
  return (
    <Section
      title="Sign-in with a provider"
      description="Admins sign in with Google, GitHub, Discord and other accounts through an OpenID Connect provider instead of pasting a token: SpacetimeAuth on maincloud, or any provider on your own server. Invite them by email; tokens keep working."
    >
      {loading ? <Loading /> : editing ? (
        <div className="max-w-xl space-y-4">
          <Field label="Issuer" hint={<>SpacetimeAuth: <code>{SPACETIMEAUTH_ISSUER}</code>; another provider: its issuer exactly as its tokens name it.</>}>
            <Input className="font-mono text-xs" value={issuer} onChange={(e) => setIssuer(e.target.value)} />
          </Field>
          <Field label="Client ID" hint="Maincloud dashboard → this database → SpacetimeAuth → Clients (the client ID is not a secret).">
            <Input className="font-mono text-xs" value={clientId} onChange={(e) => setClientId(e.target.value)} />
          </Field>
          <Field label="Redirect URI to register with the client">
            <div className="flex gap-2">
              <Input readOnly className="font-mono text-xs" value={redirectUri()} onFocus={(e) => e.target.select()} />
              <Button variant="outline" size="icon" aria-label="Copy the redirect URI" onClick={() => tryAct(() => navigator.clipboard.writeText(redirectUri()), "Copied")}><CopyIcon /></Button>
            </div>
          </Field>
          <div className="flex gap-2">
            <Button disabled={busy || !issuer.trim() || !clientId.trim()} onClick={() => save(issuer, clientId, "Admins can sign in with the provider")}>Save</Button>
            <Button variant="ghost" onClick={() => setEditing(false)}>Cancel</Button>
          </div>
        </div>
      ) : config ? (
        <div className="space-y-3 text-sm">
          <div className="flex flex-wrap items-center gap-2">
            <Tone tone="ok">on</Tone>
            <span>{providerName(config)}</span>
            <Mono className="text-muted-foreground">client {config.client_id}</Mono>
          </div>
          <p className="text-xs text-muted-foreground">Redirect URI registered with the client: <Mono className="break-all">{redirectUri()}</Mono></p>
          {canEdit && (
            <div className="flex gap-2">
              <Button variant="outline" size="sm" onClick={edit}>Change</Button>
              <Button variant="ghost" size="sm" className="text-destructive" disabled={busy} onClick={() => {
                if (confirm("Turn signing in with the provider off? Admins who joined by invitation cannot sign in until it is on again.")) void save("", "", "Signing in with a provider is off")
              }}>Turn off</Button>
            </div>
          )}
        </div>
      ) : (
        <div className="space-y-3 text-sm text-muted-foreground">
          <p>Off: admins sign in with SpacetimeDB tokens.</p>
          {canEdit && <Button variant="outline" size="sm" onClick={edit}>Set up</Button>}
        </div>
      )}
    </Section>
  )
}

/** Adds or changes a domain rule; the usage rules must be acknowledged. */
function DomainRuleForm({ rule, provider, onClose, onSaved }: { rule: DomainRuleRow | null; provider: SignInConfig; onClose: () => void; onSaved: () => void }) {
  const [domain, setDomain] = useState(rule?.domain || "")
  const [role, setRole] = useState(rule?.role || "viewer")
  const [apps, setApps] = useState<string[]>(rule ? keysOf(rule) : [])
  const [hd, setHd] = useState(rule?.require_hosted_domain ?? false)
  const [understood, setUnderstood] = useState(!!rule)
  const [busy, setBusy] = useState(false)
  const typed = domain.trim().replace(/^@/, "").toLowerCase()
  /** The domain as the texts name it. */
  const shown = typed ? `@${typed}` : "the domain's"
  const save = async () => {
    setBusy(true)
    const ok = await tryAct(
      () => call("admin_upsert_domain_rule", JSON.stringify({ domain: domain.trim(), role, game_keys: apps, require_hosted_domain: hd })),
      rule ? "Rule changed" : "Rule added",
    )
    setBusy(false)
    if (ok !== undefined) {
      onSaved()
      onClose()
    }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>{rule ? `Rule for @${rule.domain}` : "Add a domain rule"}</DialogTitle>
        <DialogDescription>
          Everyone who signs in with {providerName(provider)} using a verified {shown} address becomes an admin with this role, without an invitation.
        </DialogDescription>
      </DialogHeader>
      <DialogBody className="space-y-4">
        {!rule && <Field label="Domain"><Input placeholder="studio.com" value={domain} onChange={(e) => setDomain(e.target.value)} autoFocus /></Field>}
        <Field label="Role">
          <Select value={role} onValueChange={setRole}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{RULE_ROLES.map((r) => <SelectItem key={r.id} value={r.id}>{r.label}</SelectItem>)}</SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">{RULE_ROLES.find((r) => r.id === role)?.description}. Admins and editors are invited one by one.</p>
        </Field>
        <Field label="Apps"><AdminAppsPicker value={apps} onChange={setApps} /></Field>
        <div className="flex items-start gap-2">
          <Checkbox id="rule-hd" checked={hd} onCheckedChange={(v) => setHd(v === true)} />
          <Label htmlFor="rule-hd" className="text-sm leading-snug font-normal">
            <span>
              Only accounts of the organization: the token must carry the Google Workspace <code>hd</code> claim of {typed || "the domain"}. A personal
              account with a company address is refused. Providers that do not pass the claim (SpacetimeAuth today) let nobody in with this on.
            </span>
          </Label>
        </div>
        <Alert>
          <AlertTitle>Rules of use</AlertTitle>
          <AlertDescription>
            <ul className="list-disc space-y-1 pl-4 text-xs">
              <li>Only for a provider the company controls (Google Workspace, its own Keycloak or Okta), where leaving the company ends the account.</li>
              <li>Not with GitHub, Discord or other personal accounts: an address verified there stays verified after the person leaves.</li>
              <li>Public email domains (gmail.com, outlook.com, …) are refused: anyone can have an address there.</li>
              <li>Give the lowest role that works (Viewer). People who need more are invited one by one.</li>
              <li>Check the admin list from time to time: everyone who joined shows "via @domain". Removing one keeps them out of the rule.</li>
              <li>Remove the rule when it is no longer needed: its admins lose access at once.</li>
            </ul>
          </AlertDescription>
        </Alert>
        {!rule && (
          <div className="flex items-start gap-2">
            <Checkbox id="rule-understood" checked={understood} onCheckedChange={(v) => setUnderstood(v === true)} />
            <Label htmlFor="rule-understood" className="text-sm leading-snug font-normal">
              <span>Only people of this company can have a verified {shown} address with {providerName(provider)}.</span>
            </Label>
          </div>
        )}
      </DialogBody>
      <DialogFooter>
        <Button variant="ghost" onClick={onClose}>Cancel</Button>
        <Button onClick={save} disabled={busy || !understood || !domain.trim()}>{rule ? "Save" : "Add the rule"}</Button>
      </DialogFooter>
    </>
  )
}

function AddAdmin({ onClose, onAdded, roles, provider }: { onClose: () => void; onAdded: () => void; roles: typeof ROLES; provider: SignInConfig | null }) {
  const [email, setEmail] = useState("")
  /** The email just invited: what to tell the coworker. */
  const [invited, setInvited] = useState<string | null>(null)
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

  const invite = async () => {
    if (!email.trim()) return toast.error("Enter an email")
    setBusy(true)
    const ok = await tryAct(async () => {
      await call("admin_invite_admin", JSON.stringify({ email: email.trim(), name: name.trim(), role, game_keys: apps }))
      return true
    })
    setBusy(false)
    if (ok) {
      onAdded()
      setInvited(email.trim())
    }
  }

  const link = location.origin + location.pathname
  if (invited && provider) {
    return (
      <>
        <DialogHeader>
          <DialogTitle>Invited {invited}</DialogTitle>
          <DialogDescription>
            Send them the link. They open it, choose Sign in with {providerName(provider)} and use the account of {invited} (its email must be
            verified with the provider). The invitation is valid for 7 days and works once.
          </DialogDescription>
        </DialogHeader>
        <Field label="Admin page">
          <div className="flex gap-2">
            <Input readOnly className="font-mono text-xs" value={link} onFocus={(e) => e.target.select()} />
            <Button variant="outline" size="icon" aria-label="Copy the link" onClick={() => tryAct(() => navigator.clipboard.writeText(link), "Copied")}><CopyIcon /></Button>
          </div>
        </Field>
        <DialogFooter><Button onClick={onClose}>Done</Button></DialogFooter>
      </>
    )
  }
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
        <DialogDescription>
          {provider
            ? `Invite a coworker by email (they sign in with ${providerName(provider)}), generate a token to hand over, or add the identity of someone who already has a SpacetimeDB login.`
            : "Generate a token to hand to a coworker, or add the identity of someone who already has a SpacetimeDB login."}
        </DialogDescription>
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
        {provider && (
          <div className="flex gap-2">
            <Input type="email" placeholder="email they sign in with" value={email} onChange={(e) => setEmail(e.target.value)} onKeyDown={(e) => e.key === "Enter" && invite()} />
            <Button onClick={invite} disabled={busy || !email.trim()}><MailIcon /> Invite</Button>
          </div>
        )}
        <Button className="w-full" variant={provider ? "outline" : "default"} onClick={generate} disabled={busy}><KeyRoundIcon /> Generate a token</Button>
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
