import { useMemo, useState } from "react"
import { SearchIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { DataTable, ErrorBox, Field, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { RichTable, type RichColumn } from "@/components/rich-table"
import { SecretField } from "@/components/secret-field"
import { call, DEMO, getSetting, publicRoute, ROUTE, select } from "@/lib/api"
import { BUILTIN_SPACETIME_HOST } from "@/lib/delivery"
import { tryAct, useAsync } from "@/lib/hooks"
import { useAppIndex, useApps } from "@/lib/apps"
import { forgetRevenue } from "@/lib/revenue"
import { isFileSecret } from "@/lib/secrets"
import { buildLabel } from "@/lib/stores"
import { SECRET_NAMES } from "@/lib/types"

export function ServerPage() {
  return (
    <>
      <PageHeader title="Server" description="Settings of this billing server that are not tied to one app." />
      <div className="grid gap-6 lg:grid-cols-2">
        <HostsSection />
        <PublicAddressSection />
        <WorkerSection />
        <AnalyticsSection />
        <MetricsSection />
      </div>
      <RawSecrets />
    </>
  )
}

/** Delivery host allowlist: an app's spacetime_token is only ever sent to these hosts (https). */
function HostsSection() {
  const [host, setHost] = useState("")
  const { data, error, reload } = useAsync(() => select<{ host: string }>({ table: "spacetime_host" }))
  const rows = [{ host: BUILTIN_SPACETIME_HOST, builtin: true }, ...(data || []).map((h) => ({ host: h.host, builtin: false })).sort((a, b) => a.host.localeCompare(b.host))]
  return (
    <Section
      title="SpacetimeDB hosts"
      description={<>Deliveries, and an app's <code>spacetime_token</code>, go only to https on these hosts. Only the database owner can change the list.</>}
    >
      {error && <ErrorBox error={error} />}
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
  const { data, error, loading } = useAsync(() => getSetting("public_base_url"))
  const [value, setValue] = useState<string | null>(null)
  const current = value ?? data ?? ""
  return (
    <Section
      title="Public address"
      description="Base of the URLs shown for store notifications and clients. Set it when you open the admin through an SSH tunnel; leave it empty when this page is opened at the public address."
    >
      {error && <ErrorBox error={error} />}
      <div className="flex gap-2">
        <Input aria-label="Public address" className="font-mono text-xs" placeholder={location.origin + ROUTE} value={current} disabled={loading || !!error} onChange={(e) => setValue(e.target.value)} />
        {/* Not before the stored value loaded: saving the empty field would clear it. */}
        <Button disabled={loading || !!error} onClick={() => tryAct(() => call("admin_set_setting", "public_base_url", current.trim()), "Saved")}>Save</Button>
      </div>
    </Section>
  )
}

/** Queue drainers and how long player data is kept (`worker_lanes`, `retention_days`). */
function WorkerSection() {
  const { data, error, loading, reload } = useAsync(async () => ({ lanes: await getSetting("worker_lanes"), retention: await getSetting("retention_days") }))
  const [lanes, setLanes] = useState<string | null>(null)
  const [retention, setRetention] = useState<string | null>(null)
  const save = async (key: string, value: string) => {
    if ((await tryAct(() => call("admin_set_setting", key, value.trim()), "Saved")) !== undefined) reload()
  }
  const lanesValue = lanes ?? data?.lanes ?? ""
  const retentionValue = retention ?? data?.retention ?? ""
  return (
    <Section
      title="Worker"
      description="Queue drainers work through purchases left for later, this many at once (each holds one of the host's module instances while it waits for a store). Player data of players gone this long is deleted. Empty: the default."
    >
      {error && <ErrorBox error={error} />}
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Queue drainers (1-8, default 2)">
          <div className="flex gap-2">
            <Input inputMode="numeric" placeholder="2" value={lanesValue} disabled={loading || !!error} onChange={(e) => setLanes(e.target.value)} />
            <Button variant="outline" disabled={loading || !!error} onClick={() => save("worker_lanes", lanesValue)}>Save</Button>
          </div>
        </Field>
        <Field label="Keep player data (days)">
          <div className="flex gap-2">
            <Input inputMode="numeric" placeholder="730" value={retentionValue} disabled={loading || !!error} onChange={(e) => setRetention(e.target.value)} />
            <Button variant="outline" disabled={loading || !!error} onClick={() => save("retention_days", retentionValue)}>Save</Button>
          </div>
        </Field>
      </div>
    </Section>
  )
}

const ANALYTICS_MODES = [
  { value: "full", label: "Full", hint: "Daily totals, and the full report (payer cohorts, LTV, ARPPU, Apple Ads campaigns, trials, subscriptions running now) counted in the background a batch at a time." },
  { value: "light", label: "Light", hint: "Daily totals only: the Revenue page and Overview show them; nothing reads the purchase ledger for a report." },
  { value: "off", label: "Off (large games)", hint: "Only what purchases need: no revenue pages or reports (count revenue in Tenjin, AppsFlyer and the like, or the Cloud Storage export to BigQuery). Business alerts and /metrics keep working. Turned on again, the totals are rebuilt from the ledger." },
]

/** How much revenue analytics the server keeps (`analytics`). */
function AnalyticsSection() {
  const { data, error, loading, reload } = useAsync(() => getSetting("analytics"))
  const current = data || "full"
  const set = async (value: string) => {
    if (value === current) return
    if (value === "off" && !confirm("Turn revenue analytics off? The Revenue page and reports stay empty until you turn them on again (then the totals are rebuilt from the ledger); business alerts and /metrics keep working.")) return
    if ((await tryAct(() => call("admin_set_setting", "analytics", value), DEMO ? "Demo: this tab shows the mode (nothing is saved)" : "Saved")) !== undefined) {
      forgetRevenue()
      reload()
    }
  }
  return (
    <Section title="Analytics" description={ANALYTICS_MODES.find((m) => m.value === current)?.hint}>
      {error && <ErrorBox error={error} />}
      <Select value={current} disabled={loading || !!error} onValueChange={set}>
        <SelectTrigger className="w-48" aria-label="Analytics"><SelectValue /></SelectTrigger>
        <SelectContent>{ANALYTICS_MODES.map((m) => <SelectItem key={m.value} value={m.value}>{m.label}</SelectItem>)}</SelectContent>
      </Select>
    </Section>
  )
}

/** `GET /metrics` for Prometheus: on while the `metrics_token` secret is set. */
function MetricsSection() {
  const { data, error, loading, reload } = useAsync(async () => {
    const rows = await select<{ key: string }>({ table: "credential", columns: ["key"] })
    return rows.some((r) => r.key === "default/metrics_token")
  })
  // The public address when one is set (a scraper reaches the server there, not through this page's tunnel).
  const base = useAsync(publicRoute)
  const [shown, setShown] = useState("")
  const url = `${base.data || location.origin + ROUTE}/metrics`
  const create = async () => {
    if (data && !confirm("Create a new token? Scrapers with the old one get 401 until they are updated.")) return
    const bytes = new Uint8Array(24)
    crypto.getRandomValues(bytes)
    const token = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")
    if ((await tryAct(() => call("admin_set_credential", "default", "metrics_token", token), "Token saved")) !== undefined) {
      setShown(token)
      reload()
    }
  }
  const turnOff = async () => {
    if (!confirm("Turn /metrics off? The route answers 404 until a new token is created.")) return
    if ((await tryAct(() => call("admin_delete_credential", "default", "metrics_token"), "Metrics turned off")) !== undefined) {
      setShown("")
      reload()
    }
  }
  return (
    <Section
      title="Metrics"
      description={<>Prometheus text at <Mono className="break-all">{url}</Mono>: the queue, ledger rows by status, store notifications, integration events, open alerts and today&apos;s revenue per build. Scrapers send <code>Authorization: Bearer &lt;token&gt;</code>.</>}
    >
      {error && <ErrorBox error={error} />}
      <div className="flex flex-wrap items-center gap-2">
        <Tone tone={data ? "ok" : "muted"}>{loading ? "…" : data ? "on" : "off"}</Tone>
        <Button variant="outline" disabled={loading || !!error} onClick={create}>{data ? "New token" : "Create token"}</Button>
        {data && <Button variant="ghost" className="text-destructive" onClick={turnOff}>Turn off</Button>}
      </div>
      {shown && (
        <p className="mt-3 text-xs text-muted-foreground">
          The token (shown once): <Mono className="break-all text-foreground">{shown}</Mono>
        </p>
      )}
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
  const [find, setFind] = useState("")
  const apps = useApps().apps
  const { groupOf, builds } = useAppIndex()
  const { data, error, loading, reload } = useAsync(async () => {
    const keys = await select<{ key: string }>({ table: "credential", columns: ["key"] })
    return keys
      .map((k) => { const i = k.key.indexOf("/"); return { scope: k.key.slice(0, i), name: k.key.slice(i + 1) } })
      .sort((a, b) => (a.scope + a.name).localeCompare(b.scope + b.name))
  })
  // Scope suggestions: default, the developer accounts, then every build with its app's name.
  const { scopes, accounts } = useMemo(() => {
    const out = new Map<string, string>([["default", "fallback for every build"]])
    const accounts = new Set<string>()
    for (const a of apps || []) {
      if (!a.account || a.account === "default") continue
      accounts.add(a.account)
      out.set(a.account, "developer account")
    }
    for (const a of apps || []) if (!out.has(a.app_id)) out.set(a.app_id, `${groupOf.get(a.app_id)?.name ?? a.game_key} · ${buildLabel(a)}`)
    return { scopes: [...out], accounts }
  }, [apps, groupOf])
  // What a scope is: a build of an app or a developer account.
  const scopeText = (scope: string) => {
    const b = builds.get(scope)
    if (b) return `${groupOf.get(scope)?.name ?? b.game_key} · ${buildLabel(b)}`
    if (/^alert-\d+$/.test(scope)) return "alert channel"
    return accounts.has(scope) ? "developer account" : ""
  }
  const words = find.toLowerCase().split(/\s+/).filter(Boolean)
  const rows = (data || []).filter((r) => {
    if (!words.length) return true
    const hay = `${r.scope}/${r.name} ${scopeText(r.scope)}`.toLowerCase()
    return words.every((w) => hay.includes(w))
  })
  const columns: RichColumn<{ scope: string; name: string }>[] = [
    {
      title: "Scope",
      size: 220,
      sort: (r) => r.scope,
      cell: (r) => {
        const what = scopeText(r.scope)
        return (
          <span className="flex min-w-0 flex-col">
            <Mono className="truncate">{r.scope}</Mono>
            {what && <span className="truncate text-xs text-muted-foreground">{what}</span>}
          </span>
        )
      },
    },
    { title: "Name", size: 180, sort: (r) => r.name, cell: (r) => <Mono>{r.name}</Mono> },
    {
      id: "actions",
      title: "",
      size: 52,
      className: "text-right",
      cell: (r) => (
        <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Delete" onClick={async (e) => {
          e.stopPropagation()
          if (!confirm(`Delete secret ${r.scope}/${r.name}?`)) return
          if (await tryAct(() => call("admin_delete_credential", r.scope, r.name), "Deleted") !== undefined) reload()
        }}><Trash2Icon /></Button>
      ),
    },
  ]
  const save = async () => {
    if (!scope.trim() || !value.trim()) return toast.error("Scope and value are required")
    const ok = await tryAct(() => call("admin_set_credential", scope.trim(), name, value), `Secret ${scope}/${name} saved`)
    if (ok !== undefined) { setValue(""); reload() }
  }
  return (
    <div className="mt-4 grid gap-6 lg:grid-cols-[3fr_2fr]">
      <Section title="Stored secrets" description={<>Lookup order: store app → developer account → <code>default</code>. Values are never shown.</>}>
        {error ? <ErrorBox error={error} /> : loading || !data ? <Loading /> : (
          <div className="space-y-3">
            {data.length > 0 && (
              <div className="relative">
                <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
                <Input className="pl-8" placeholder="Filter by scope, app name or secret name" value={find} onChange={(e) => setFind(e.target.value)} aria-label="Filter secrets" />
              </div>
            )}
            <RichTable
              rows={rows}
              columns={columns}
              rowKey={(r) => `${r.scope}/${r.name}`}
              noun="secrets"
              pageSize={25}
              initialSort={{ id: "Scope" }}
              empty={data.length ? "No secret matches." : "No secrets yet."}
            />
          </div>
        )}
      </Section>
      <Section title="Set or replace a secret">
        <div className="grid gap-4">
          <Field label="Scope (store app id, account or default)">
            <Input list="scopes" value={scope} onChange={(e) => setScope(e.target.value)} />
            <datalist id="scopes">{scopes.map(([s, label]) => <option key={s} value={s} label={label} />)}</datalist>
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
