import { useState } from "react"
import { Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { SecretField } from "@/components/secret-field"
import { call, getSetting, ROUTE, select } from "@/lib/api"
import { BUILTIN_SPACETIME_HOST } from "@/lib/delivery"
import { tryAct, useAsync } from "@/lib/hooks"
import { useApps } from "@/lib/apps"
import { isFileSecret } from "@/lib/secrets"
import { SECRET_NAMES } from "@/lib/types"

export function ServerPage() {
  return (
    <>
      <PageHeader title="Server" description="Settings of this billing server that are not tied to one app." />
      <div className="grid gap-6 lg:grid-cols-2">
        <HostsSection />
        <PublicAddressSection />
      </div>
      <RawSecrets />
    </>
  )
}

/** Delivery host allowlist: an app's spacetime_token is only ever sent to these hosts (https). */
function HostsSection() {
  const [host, setHost] = useState("")
  const { data, reload } = useAsync(() => select<{ host: string }>({ table: "spacetime_host" }))
  const rows = [{ host: BUILTIN_SPACETIME_HOST, builtin: true }, ...(data || []).map((h) => ({ host: h.host, builtin: false })).sort((a, b) => a.host.localeCompare(b.host))]
  return (
    <Section
      title="SpacetimeDB hosts"
      description={<>Deliveries, and an app's <code>spacetime_token</code>, go only to https on these hosts. Only the database owner can change the list.</>}
    >
      <DataTable
        rows={rows}
        columns={[
          { title: "Host", cell: (r) => <span className="flex items-center gap-2"><Mono>{r.host}</Mono>{r.builtin && <Tone tone="muted">built in</Tone>}</span> },
          {
            title: "",
            className: "text-right",
            cell: (r) => !r.builtin && (
              <Button variant="ghost" size="xs" className="text-destructive" onClick={async () => {
                if (!confirm(`Stop deliveries to ${r.host}? Apps on this host fail delivery until moved.`)) return
                if (await tryAct(() => call("admin_remove_spacetime_host", r.host), "Removed") !== undefined) reload()
              }}>Remove</Button>
            ),
          },
        ]}
      />
      <div className="mt-4 flex gap-2">
        <Input placeholder="stdb.example.com" className="font-mono text-xs" value={host} onChange={(e) => setHost(e.target.value)} />
        <Button disabled={!host.trim()} onClick={async () => {
          if (await tryAct(() => call("admin_add_spacetime_host", host.trim()), "Allowed") === undefined) return
          setHost("")
          reload()
        }}>Allow</Button>
      </div>
    </Section>
  )
}

function PublicAddressSection() {
  const { data } = useAsync(() => getSetting("public_base_url"))
  const [value, setValue] = useState<string | null>(null)
  const current = value ?? data ?? ""
  return (
    <Section
      title="Public address"
      description="Base of the URLs shown for store notifications and clients. Set it when you open the admin through an SSH tunnel; leave it empty when this page is opened at the public address."
    >
      <div className="flex gap-2">
        <Input className="font-mono text-xs" placeholder={location.origin + ROUTE} value={current} onChange={(e) => setValue(e.target.value)} />
        <Button onClick={() => tryAct(() => call("admin_set_setting", "public_base_url", current.trim()), "Saved")}>Save</Button>
      </div>
    </Section>
  )
}

/** Every stored secret by its raw `{scope}/{name}` key: loaded when the section is opened. */
function RawSecrets() {
  const [opened, setOpened] = useState(false)
  return (
    <details className="group" onToggle={(e) => e.currentTarget.open && setOpened(true)}>
      <summary className="cursor-pointer text-sm font-medium text-muted-foreground">Advanced: all secrets by scope</summary>
      {opened && <RawSecretsBody />}
    </details>
  )
}

function RawSecretsBody() {
  const [scope, setScope] = useState("default")
  const [name, setName] = useState(SECRET_NAMES[0][0])
  const [value, setValue] = useState("")
  const apps = useApps().apps
  const { data, error, loading, reload } = useAsync(async () => {
    const keys = await select<{ key: string }>({ table: "credential", columns: ["key"] })
    return keys
      .map((k) => { const i = k.key.indexOf("/"); return { scope: k.key.slice(0, i), name: k.key.slice(i + 1) } })
      .sort((a, b) => (a.scope + a.name).localeCompare(b.scope + b.name))
  })
  const scopes = [...new Set(["default", ...(apps || []).map((a) => a.account), ...(apps || []).map((a) => a.app_id)])]
  const save = async () => {
    if (!scope.trim() || !value.trim()) return toast.error("Scope and value are required")
    const ok = await tryAct(() => call("admin_set_credential", scope.trim(), name, value), `Secret ${scope}/${name} saved`)
    if (ok !== undefined) { setValue(""); reload() }
  }
  return (
    <div className="mt-4 grid gap-6 lg:grid-cols-[3fr_2fr]">
      <Section title="Stored secrets" description={<>Lookup order: store app → developer account → <code>default</code>. Values are never shown.</>}>
        {error ? <ErrorBox error={error} /> : loading || !data ? <Loading /> : (
          <DataTable
            rows={data}
            empty="No secrets yet."
            columns={[
              { title: "Scope", cell: (r) => <Mono>{r.scope}</Mono> },
              { title: "Name", cell: (r) => <Mono>{r.name}</Mono> },
              {
                title: "",
                className: "text-right",
                cell: (r) => (
                  <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Delete" onClick={async () => {
                    if (!confirm(`Delete secret ${r.scope}/${r.name}?`)) return
                    if (await tryAct(() => call("admin_delete_credential", r.scope, r.name), "Deleted") !== undefined) reload()
                  }}><Trash2Icon /></Button>
                ),
              },
            ]}
          />
        )}
      </Section>
      <Section title="Set or replace a secret">
        <div className="grid gap-4">
          <Field label="Scope (store app id, account or default)">
            <Input list="scopes" value={scope} onChange={(e) => setScope(e.target.value)} />
            <datalist id="scopes">{scopes.map((s) => <option key={s} value={s} />)}</datalist>
          </Field>
          <Field label="Name" hint={SECRET_NAMES.find(([n]) => n === name)?.[1]}>
            <Select value={name} onValueChange={setName}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{SECRET_NAMES.map(([n]) => <SelectItem key={n} value={n}>{n}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
          <Field label="Value">
            <SecretField value={value} onChange={setValue} file={isFileSecret(name)} />
          </Field>
          <Button onClick={save}>Save secret</Button>
        </div>
      </Section>
    </div>
  )
}
