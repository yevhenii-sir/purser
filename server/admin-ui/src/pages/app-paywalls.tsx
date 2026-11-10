import { useMemo, useState } from "react"
import { ArrowDownIcon, ArrowUpIcon, ChartColumnIcon, EyeIcon, FlaskConicalIcon, PlusIcon, SquareIcon, Trash2Icon, XIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"
import { DataTable, DialogBody, ErrorBox, Field, Loading, Mono, Section, Tone } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { call, proc, sql, sqlStr, sqlWhereIn } from "@/lib/api"
import { PLATFORM_LABEL } from "@/lib/apps"
import { errorText, fmtTime, usd } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import type { Product } from "@/lib/types"
import type { TabProps } from "@/pages/app"

interface PaywallRow {
  key: string
  game_key: string
  paywall_id: string
  name: string
  product_ids_json: string
  remote_config_json: string
  updated_at_micros: number
}

interface Variant {
  variant_id: string
  paywall_id: string
  weight: number
}

interface PlacementRow {
  key: string
  game_key: string
  placement_id: string
  variants_json: string
  test_id: string
  test_started_at_micros: number
  test_stopped_at_micros: number
  enabled: boolean
  updated_at_micros: number
}

interface TestRow {
  test_id: string
  placement_key: string
  variants_json: string
  started_at_micros: number
  stopped_at_micros: number
  winner_variant_id: string
}

interface VariantResult extends Variant {
  views: number
  purchases: number
  renewals: number
  refunds: number
  conversion: number | null
  gross_usd_micros: number
  net_usd_micros: number
  arpu_per_view_usd_micros: number | null
  probability_best: number
}

interface Results {
  variants: VariantResult[]
  totals: { views: number; purchases: number; conversion: number | null; gross_usd_micros: number; net_usd_micros: number }
  unknown_variant_purchases: number
  fx: { missing: string[] }
  test: { test_id: string; running: boolean; started_at_ms: number; stopped_at_ms: number; winner_variant_id: string } | null
  from?: string
  to?: string
}

const ID_RE = /^[A-Za-z0-9_.-]{1,64}$/
const parseList = <T,>(json: string): T[] => {
  try {
    const v = JSON.parse(json)
    return Array.isArray(v) ? v : []
  } catch {
    return []
  }
}
const running = (p: PlacementRow) => !!p.test_id && !Number(p.test_stopped_at_micros)
const pct = (x: number | null | undefined, digits = 1) => (x === null || x === undefined ? "—" : `${(x * 100).toFixed(digits)}%`)
const shares = (vs: Variant[]) => {
  const total = vs.reduce((s, v) => s + Number(v.weight || 0), 0)
  return vs.map((v) => (total ? Number(v.weight || 0) / total : 0))
}

export function AppPaywallsTab({ data }: TabProps) {
  const g = data.group
  const ids = g.apps.map((a) => a.app_id)
  const loaded = useAsync(async () => {
    const [paywalls, placements, products] = await Promise.all([
      sql<PaywallRow>(`SELECT * FROM paywall WHERE game_key = ${sqlStr(g.key)}`),
      sql<PlacementRow>(`SELECT * FROM placement WHERE game_key = ${sqlStr(g.key)}`),
      sqlWhereIn<Product>("product", "app_id", ids),
    ])
    // Tests by the indexed placement key (history of deleted placements is not listed).
    const tests = await sqlWhereIn<TestRow>("paywall_test", "placement_key", placements.map((p) => p.key))
    return {
      paywalls: paywalls.sort((a, b) => a.paywall_id.localeCompare(b.paywall_id)),
      placements: placements.sort((a, b) => a.placement_id.localeCompare(b.placement_id)),
      tests: tests.sort((a, b) => Number(b.started_at_micros) - Number(a.started_at_micros)),
      products,
    }
  }, [g.key, ids.join(",")])
  const [paywall, setPaywall] = useState<PaywallRow | "new" | null>(null)
  const [placement, setPlacement] = useState<PlacementRow | "new" | null>(null)
  const [testDialog, setTestDialog] = useState<{ p: PlacementRow; mode: "start" | "stop" } | null>(null)
  const [preview, setPreview] = useState<PlacementRow | null>(null)
  const [resultsFor, setResultsFor] = useState<string>("")

  if (loaded.error) return <ErrorBox error={loaded.error} />
  if (loaded.loading && !loaded.data) return <Loading rows={6} />
  const d = loaded.data!
  const catalog = [...new Set(d.products.map((p) => p.product_id))].sort()
  const usedBy = (paywallId: string) => d.placements.filter((p) => parseList<Variant>(p.variants_json).some((v) => v.paywall_id === paywallId)).map((p) => p.placement_id)
  const selected = d.placements.find((p) => p.placement_id === resultsFor) || d.placements[0]

  const removePaywall = async (p: PaywallRow) => {
    if (!confirm(`Delete paywall ${p.paywall_id}?`)) return
    if ((await tryAct(() => call("admin_delete_paywall", g.key, p.paywall_id), "Deleted")) !== undefined) loaded.reload()
  }
  const removePlacement = async (p: PlacementRow) => {
    if (!confirm(`Delete placement ${p.placement_id}? The game gets "not found" for it; test history is kept.`)) return
    if ((await tryAct(() => call("admin_delete_placement", g.key, p.placement_id), "Deleted")) !== undefined) loaded.reload()
  }

  return (
    <>
      <Section
        title="Placements"
        description="Where the game shows a paywall (the id it passes to GetPaywallAsync). Without a running test every player gets the first variant; during a test players are split by the weights, always into the same variant."
        actions={<Button size="sm" onClick={() => setPlacement("new")} disabled={!d.paywalls.length}><PlusIcon /> Add placement</Button>}
      >
        <DataTable
          rows={d.placements}
          onRowClick={setPlacement}
          rowKey={(p) => p.key}
          empty={d.paywalls.length ? "No placements yet." : "Create a paywall first, then a placement that shows it."}
          columns={[
            { title: "Placement", cell: (p) => <Mono>{p.placement_id}</Mono> },
            {
              title: "Variants",
              cell: (p) => {
                const vs = parseList<Variant>(p.variants_json)
                const sh = shares(vs)
                return (
                  <div className="flex flex-wrap gap-x-3 gap-y-1 text-xs">
                    {vs.map((v, i) => (
                      <span key={v.variant_id} className="min-w-0">
                        <Mono>{v.variant_id}</Mono> → <Mono>{v.paywall_id}</Mono>
                        {running(p) ? <span className="text-muted-foreground"> {pct(sh[i], 0)}</span> : i === 0 ? <span className="text-muted-foreground"> (served)</span> : null}
                      </span>
                    ))}
                  </div>
                )
              },
            },
            {
              title: "Status",
              cell: (p) => !p.enabled ? <Tone tone="muted">disabled</Tone> : running(p) ? <Tone tone="ok">test {p.test_id}</Tone> : <Tone tone="muted">no test</Tone>,
            },
            {
              title: "",
              className: "text-right",
              cell: (p) => (
                <span className="inline-flex gap-1">
                  <Button size="xs" variant="outline" onClick={() => setPreview(p)}><EyeIcon /> Preview</Button>
                  {running(p)
                    ? <Button size="xs" variant="outline" onClick={() => setTestDialog({ p, mode: "stop" })}><SquareIcon /> Stop test</Button>
                    : <Button size="xs" variant="outline" onClick={() => setTestDialog({ p, mode: "start" })} disabled={parseList<Variant>(p.variants_json).length < 2}><FlaskConicalIcon /> Start test</Button>}
                  <Button size="icon-sm" variant="ghost" className="text-destructive" aria-label="Delete placement" onClick={() => removePlacement(p)}><Trash2Icon /></Button>
                </span>
              ),
            },
          ]}
        />
      </Section>

      <Section
        title="Paywalls"
        description="Products in display order and a remote config (any JSON) the game reads, changed without an app update. Each build gets the products configured for it."
        actions={<Button size="sm" onClick={() => setPaywall("new")} disabled={!catalog.length}><PlusIcon /> Add paywall</Button>}
      >
        <DataTable
          rows={d.paywalls}
          onRowClick={setPaywall}
          rowKey={(p) => p.key}
          empty={catalog.length ? "No paywalls yet." : "Add products first (Products tab)."}
          columns={[
            { title: "Paywall", cell: (p) => <span className="flex min-w-0 flex-col"><Mono>{p.paywall_id}</Mono>{p.name !== p.paywall_id && <span className="truncate text-xs text-muted-foreground">{p.name}</span>}</span> },
            {
              title: "Products",
              cell: (p) => {
                const list = parseList<string>(p.product_ids_json)
                return list.length ? <span className="text-xs">{list.map((id, i) => <span key={id}>{i > 0 && ", "}<Mono>{id}</Mono>{!catalog.includes(id) && <Tone tone="warn">removed</Tone>}</span>)}</span> : <span className="text-muted-foreground">—</span>
              },
            },
            { title: "Remote config", cell: (p) => <Mono className="block max-w-64 truncate text-muted-foreground">{p.remote_config_json}</Mono> },
            { title: "Used by", cell: (p) => usedBy(p.paywall_id).join(", ") || <span className="text-muted-foreground">—</span> },
            { title: "Updated", cell: (p) => <span className="text-xs text-muted-foreground">{fmtTime(p.updated_at_micros)}</span> },
            { title: "", className: "text-right", cell: (p) => <Button size="icon-sm" variant="ghost" className="text-destructive" aria-label="Delete paywall" onClick={() => removePaywall(p)}><Trash2Icon /></Button> },
          ]}
        />
      </Section>

      {selected && (
        <ResultsSection
          gameKey={g.key}
          placements={d.placements}
          placement={selected}
          tests={d.tests.filter((t) => t.placement_key === selected.key)}
          onPlacement={setResultsFor}
        />
      )}

      <Dialog open={!!paywall} onOpenChange={(o) => !o && setPaywall(null)}>
        <DialogContent className="sm:max-w-2xl">
          {paywall && (
            <PaywallForm key={paywall === "new" ? "new" : paywall.key} gameKey={g.key} row={paywall === "new" ? null : paywall}
              catalog={catalog} products={d.products} apps={g.apps.map((a) => ({ id: a.app_id, label: PLATFORM_LABEL[a.platform] }))}
              onClose={() => setPaywall(null)} onSaved={loaded.reload} />
          )}
        </DialogContent>
      </Dialog>
      <Dialog open={!!placement} onOpenChange={(o) => !o && setPlacement(null)}>
        <DialogContent className="sm:max-w-2xl">
          {placement && (
            <PlacementForm key={placement === "new" ? "new" : placement.key} gameKey={g.key} row={placement === "new" ? null : placement}
              paywalls={d.paywalls.map((p) => p.paywall_id)} onClose={() => setPlacement(null)} onSaved={loaded.reload} />
          )}
        </DialogContent>
      </Dialog>
      <Dialog open={!!testDialog} onOpenChange={(o) => !o && setTestDialog(null)}>
        <DialogContent className="sm:max-w-md">
          {testDialog && <TestForm gameKey={g.key} {...testDialog} onClose={() => setTestDialog(null)} onSaved={loaded.reload} />}
        </DialogContent>
      </Dialog>
      <Dialog open={!!preview} onOpenChange={(o) => !o && setPreview(null)}>
        <DialogContent className="sm:max-w-2xl">
          {preview && <PreviewForm placement={preview} apps={g.apps.map((a) => ({ id: a.app_id, label: PLATFORM_LABEL[a.platform] }))} />}
        </DialogContent>
      </Dialog>
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Paywall editor: ordered products from the app's catalog and the remote config
// ---------------------------------------------------------------------------------------------

function PaywallForm({ gameKey, row, catalog, products, apps, onClose, onSaved }: {
  gameKey: string
  row: PaywallRow | null
  catalog: string[]
  products: Product[]
  apps: { id: string; label: string }[]
  onClose: () => void
  onSaved: () => void
}) {
  const [id, setId] = useState(row?.paywall_id || "")
  const [name, setName] = useState(row?.name || "")
  const [list, setList] = useState<string[]>(() => (row ? parseList<string>(row.product_ids_json) : []))
  const [config, setConfig] = useState(() => {
    if (!row) return "{\n  \n}"
    try { return JSON.stringify(JSON.parse(row.remote_config_json), null, 2) } catch { return row.remote_config_json }
  })
  const configError = useMemo(() => {
    if (!config.trim()) return ""
    try { JSON.parse(config); return "" } catch (e) { return errorText(e) }
  }, [config])
  const available = catalog.filter((p) => !list.includes(p))
  const move = (i: number, by: number) => setList((l) => {
    const n = [...l]
    const [x] = n.splice(i, 1)
    n.splice(i + by, 0, x)
    return n
  })
  const missingIn = (pid: string) => apps.filter((a) => !products.some((p) => p.app_id === a.id && p.product_id === pid)).map((a) => a.label)

  const save = async () => {
    const pid = id.trim()
    if (!ID_RE.test(pid)) return toast.error("Paywall id: 1-64 of A-Z a-z 0-9 _ - .")
    if (configError) return toast.error("Remote config is not valid JSON")
    const ok = await tryAct(() => call("admin_upsert_paywall", JSON.stringify({ game_key: gameKey, paywall_id: pid, name: name.trim(), product_ids: list, remote_config_json: config })), "Saved")
    if (ok !== undefined) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>{row ? row.paywall_id : "New paywall"}</DialogTitle>
        <DialogDescription>Players see the products in this order. The remote config is any JSON your paywall screen reads (titles, colors, layout, discounts).</DialogDescription>
      </DialogHeader>
      <DialogBody className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Paywall id"><Input className="font-mono text-xs" value={id} onChange={(e) => setId(e.target.value)} readOnly={!!row} placeholder="pw_default" /></Field>
          <Field label="Name"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="Default paywall" /></Field>
        </div>
        <Field label="Products">
          <div className="space-y-1">
            {list.map((pid, i) => {
              const missing = missingIn(pid)
              return (
                <div key={pid} className="flex items-center gap-2 rounded-md border px-2 py-1">
                  <span className="w-5 shrink-0 text-xs text-muted-foreground tabular-nums">{i + 1}</span>
                  <Mono className="min-w-0 flex-1 truncate">{pid}</Mono>
                  {missing.length > 0 && <span className="shrink-0"><Tone tone="warn">not on {missing.join(", ")}</Tone></span>}
                  <Button size="icon-xs" variant="ghost" aria-label="Move up" disabled={i === 0} onClick={() => move(i, -1)}><ArrowUpIcon /></Button>
                  <Button size="icon-xs" variant="ghost" aria-label="Move down" disabled={i === list.length - 1} onClick={() => move(i, 1)}><ArrowDownIcon /></Button>
                  <Button size="icon-xs" variant="ghost" aria-label="Remove" onClick={() => setList((l) => l.filter((x) => x !== pid))}><XIcon /></Button>
                </div>
              )
            })}
            {available.length > 0 && (
              <Select value="" onValueChange={(v) => setList((l) => [...l, v])}>
                <SelectTrigger className="w-full"><SelectValue placeholder="Add a product from the catalog…" /></SelectTrigger>
                <SelectContent>{available.map((p) => <SelectItem key={p} value={p}>{p}</SelectItem>)}</SelectContent>
              </Select>
            )}
          </div>
        </Field>
        <Field label="Remote config (JSON)" hint={configError ? <span className="text-destructive">{configError}</span> : "Up to 16 KB. Empty means {}."}>
          <Textarea className="min-h-44 font-mono text-xs" value={config} onChange={(e) => setConfig(e.target.value)} aria-invalid={!!configError} spellCheck={false} />
        </Field>
      </DialogBody>
      <DialogFooter className="gap-2 sm:justify-between">
        <Button variant="outline" disabled={!!configError || !config.trim()} onClick={() => setConfig(JSON.stringify(JSON.parse(config), null, 2))}>Format JSON</Button>
        <Button onClick={save}>Save</Button>
      </DialogFooter>
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Placement editor: variants and weights
// ---------------------------------------------------------------------------------------------

function PlacementForm({ gameKey, row, paywalls, onClose, onSaved }: {
  gameKey: string
  row: PlacementRow | null
  paywalls: string[]
  onClose: () => void
  onSaved: () => void
}) {
  const locked = !!row && running(row)
  const [id, setId] = useState(row?.placement_id || "")
  const [enabled, setEnabled] = useState(row ? !!row.enabled : true)
  const [variants, setVariants] = useState<Variant[]>(() =>
    row ? parseList<Variant>(row.variants_json) : [{ variant_id: "a", paywall_id: paywalls[0] || "", weight: 1 }])
  const sh = shares(variants)
  const set = (i: number, patch: Partial<Variant>) => setVariants((vs) => vs.map((v, j) => (j === i ? { ...v, ...patch } : v)))
  const nextId = () => "abcdefghij".split("").find((c) => !variants.some((v) => v.variant_id === c)) || `v${variants.length + 1}`

  const save = async () => {
    const pid = id.trim()
    if (!ID_RE.test(pid)) return toast.error("Placement id: 1-64 of A-Z a-z 0-9 _ - .")
    if (variants.some((v) => !ID_RE.test(v.variant_id) || !v.paywall_id)) return toast.error("Every variant needs an id and a paywall")
    const ok = await tryAct(() => call("admin_upsert_placement", JSON.stringify({
      game_key: gameKey, placement_id: pid, enabled, variants: variants.map((v) => ({ ...v, weight: Math.max(0, Math.round(Number(v.weight) || 0)) })),
    })), "Saved")
    if (ok !== undefined) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>{row ? row.placement_id : "New placement"}</DialogTitle>
        <DialogDescription>
          The first variant is served when no test runs. Weights split players during a test (1 and 1 is 50/50).
          {locked && " A test is running: stop it to change the variants."}
        </DialogDescription>
      </DialogHeader>
      <DialogBody className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Placement id"><Input className="font-mono text-xs" value={id} onChange={(e) => setId(e.target.value)} readOnly={!!row} placeholder="main_paywall" /></Field>
          <div className="flex items-end gap-2 pb-2">
            <Switch id="placement-enabled" checked={enabled} onCheckedChange={setEnabled} />
            <Label htmlFor="placement-enabled" className="font-normal">Enabled (disabled placements answer "not found")</Label>
          </div>
        </div>
        <div className="space-y-2">
          {variants.map((v, i) => (
            <div key={i} className="grid grid-cols-[minmax(0,5rem)_minmax(0,1fr)_minmax(0,5rem)_auto_auto] items-center gap-2">
              <Input className="font-mono text-xs" value={v.variant_id} disabled={locked} aria-label="Variant id" onChange={(e) => set(i, { variant_id: e.target.value })} />
              <Select value={v.paywall_id} disabled={locked} onValueChange={(p) => set(i, { paywall_id: p })}>
                <SelectTrigger className="w-full min-w-0"><SelectValue placeholder="Paywall" /></SelectTrigger>
                <SelectContent>{paywalls.map((p) => <SelectItem key={p} value={p}>{p}</SelectItem>)}</SelectContent>
              </Select>
              <Input type="number" min="0" step="1" value={v.weight} disabled={locked} aria-label="Weight" onChange={(e) => set(i, { weight: Number(e.target.value) })} />
              <span className="w-12 shrink-0 text-right text-xs text-muted-foreground tabular-nums">{pct(sh[i], 0)}</span>
              <Button size="icon-sm" variant="ghost" aria-label="Remove variant" disabled={locked || variants.length === 1} onClick={() => setVariants((vs) => vs.filter((_, j) => j !== i))}><XIcon /></Button>
            </div>
          ))}
          <Button size="sm" variant="outline" disabled={locked || variants.length >= 10} onClick={() => setVariants((vs) => [...vs, { variant_id: nextId(), paywall_id: paywalls[0] || "", weight: 1 }])}>
            <PlusIcon /> Add variant
          </Button>
        </div>
      </DialogBody>
      <DialogFooter><Button onClick={save}>Save</Button></DialogFooter>
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Start / stop a test
// ---------------------------------------------------------------------------------------------

function TestForm({ gameKey, p, mode, onClose, onSaved }: { gameKey: string; p: PlacementRow; mode: "start" | "stop"; onClose: () => void; onSaved: () => void }) {
  const variants = parseList<Variant>(p.variants_json)
  const sh = shares(variants)
  const [testId, setTestId] = useState("")
  const [winner, setWinner] = useState("__keep")
  const submit = async () => {
    const ok = mode === "start"
      ? await tryAct(() => call("admin_start_paywall_test", gameKey, p.placement_id, testId.trim()), "Test started")
      : await tryAct(() => call("admin_stop_paywall_test", gameKey, p.placement_id, winner === "__keep" ? "" : winner), "Test stopped")
    if (ok !== undefined) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>{mode === "start" ? `Start a test on ${p.placement_id}` : `Stop test ${p.test_id}`}</DialogTitle>
        <DialogDescription>
          {mode === "start"
            ? `Players are split ${variants.map((v, i) => `${v.variant_id} ${pct(sh[i], 0)}`).join(" / ")} and keep their variant until the test stops.`
            : "Every player gets the chosen variant from now on (players without a fresh answer keep their cached one until the next request)."}
        </DialogDescription>
      </DialogHeader>
      {mode === "start" ? (
        <Field label="Test id" hint="Empty: generated from the placement and today's date. Ids are never reused.">
          <Input className="font-mono text-xs" value={testId} onChange={(e) => setTestId(e.target.value)} placeholder={`${p.placement_id}-${new Date().toISOString().slice(0, 10).replace(/-/g, "")}`} />
        </Field>
      ) : (
        <Field label="Serve after the test">
          <Select value={winner} onValueChange={setWinner}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="__keep">Keep the first variant ({variants[0]?.variant_id})</SelectItem>
              {variants.map((v) => <SelectItem key={v.variant_id} value={v.variant_id}>Winner: {v.variant_id} ({v.paywall_id})</SelectItem>)}
            </SelectContent>
          </Select>
        </Field>
      )}
      <DialogFooter><Button onClick={submit}>{mode === "start" ? "Start test" : "Stop test"}</Button></DialogFooter>
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Preview: what a build and player get, without counting a view
// ---------------------------------------------------------------------------------------------

function PreviewForm({ placement, apps }: { placement: PlacementRow; apps: { id: string; label: string }[] }) {
  const [app, setApp] = useState(apps[0]?.id || "")
  const [user, setUser] = useState("player-1")
  const [answer, setAnswer] = useState<string>("")
  const run = async () => {
    try {
      setAnswer(JSON.stringify(await proc("admin_preview_paywall", app, placement.placement_id, user.trim()), null, 2))
    } catch (e) {
      setAnswer(errorText(e))
    }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle>Preview {placement.placement_id}</DialogTitle>
        <DialogDescription>The <code>/paywall</code> answer for a build and player id. No view is counted.</DialogDescription>
      </DialogHeader>
      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] sm:items-end">
        <Field label="Build">
          <Select value={app} onValueChange={setApp}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{apps.map((a) => <SelectItem key={a.id} value={a.id}>{a.label} {a.id}</SelectItem>)}</SelectContent>
          </Select>
        </Field>
        <Field label="User id"><Input className="font-mono text-xs" value={user} onChange={(e) => setUser(e.target.value)} /></Field>
        <Button onClick={run}>Preview</Button>
      </div>
      {answer && <DialogBody><pre className="rounded-md bg-muted p-3 font-mono text-xs whitespace-pre-wrap break-all">{answer}</pre></DialogBody>}
    </>
  )
}

// ---------------------------------------------------------------------------------------------
// Results of a test (or of the traffic outside tests)
// ---------------------------------------------------------------------------------------------

function ResultsSection({ gameKey, placements, placement, tests, onPlacement }: {
  gameKey: string
  placements: PlacementRow[]
  placement: PlacementRow
  tests: TestRow[]
  onPlacement: (id: string) => void
}) {
  const [test, setTest] = useState<string>("")
  const [sandbox, setSandbox] = useState(false)
  const testId = test === "__none" ? "" : test || tests[0]?.test_id || ""
  // Results are computed on the server from the ledger: only on request, then on every filter change.
  const [shown, setShown] = useState(false)
  const results = useAsync(
    async () => (shown ? proc<Results>("admin_paywall_results", JSON.stringify({ game_key: gameKey, placement_id: placement.placement_id, test_id: testId, include_sandbox: sandbox })) : null),
    [shown, gameKey, placement.placement_id, testId, sandbox],
  )
  const r = results.data
  const bestP = r ? Math.max(...r.variants.map((v) => v.probability_best)) : 0
  return (
    <Section
      title="Results"
      description="Conversion per view (views come from LogPaywallShownAsync), first purchases and their renewals in USD at the rate of the purchase day, and the probability that a variant has the best conversion (Bayesian, uniform prior)."
      actions={
        <>
          <Select value={placement.placement_id} onValueChange={(v) => { onPlacement(v); setTest("") }}>
            <SelectTrigger size="sm" className="w-44"><SelectValue /></SelectTrigger>
            <SelectContent>{placements.map((p) => <SelectItem key={p.key} value={p.placement_id}>{p.placement_id}</SelectItem>)}</SelectContent>
          </Select>
          <Select value={testId || "__none"} onValueChange={setTest}>
            <SelectTrigger size="sm" className="w-56"><SelectValue /></SelectTrigger>
            <SelectContent>
              {tests.map((t) => <SelectItem key={t.test_id} value={t.test_id}>{t.test_id}{Number(t.stopped_at_micros) ? "" : " (running)"}</SelectItem>)}
              <SelectItem value="__none">Outside tests, last 30 days</SelectItem>
            </SelectContent>
          </Select>
          <div className="flex items-center gap-2">
            <Checkbox id="paywall-sandbox" checked={sandbox} onCheckedChange={(v) => setSandbox(v === true)} />
            <Label htmlFor="paywall-sandbox" className="font-normal">Sandbox</Label>
          </div>
        </>
      }
    >
      {!shown ? (
        <div className="flex flex-wrap items-center gap-3">
          <Button size="sm" variant="outline" onClick={() => setShown(true)}><ChartColumnIcon /> Show results</Button>
          <span className="text-sm text-muted-foreground">Computed from the purchase ledger when you ask for them.</span>
        </div>
      ) : results.error ? <ErrorBox error={results.error} /> : !r || results.loading ? <Loading /> : (
        <div className="space-y-3">
          {r.test && (
            <p className="text-sm text-muted-foreground">
              Test <Mono>{r.test.test_id}</Mono> {r.test.running ? "running since" : "ran"} {new Date(r.test.started_at_ms).toLocaleString()}
              {!r.test.running && <> to {new Date(r.test.stopped_at_ms).toLocaleString()}</>}
              {r.test.winner_variant_id && <> · winner <Mono>{r.test.winner_variant_id}</Mono></>}
            </p>
          )}
          <RichTable
            rows={r.variants}
            rowKey={(v) => v.variant_id}
            empty="No variants."
            noun="variants"
            columns={[
              { title: "Variant", sort: (v) => v.variant_id, cell: (v) => <span className="flex flex-col"><Mono>{v.variant_id}</Mono><span className="text-xs text-muted-foreground">{v.paywall_id}</span></span> },
              { title: "Views", num: true, sort: (v) => v.views, cell: (v) => v.views.toLocaleString() },
              { title: "Purchases", num: true, sort: (v) => v.purchases, cell: (v) => <>{v.purchases.toLocaleString()}{v.refunds > 0 && <span className="text-xs text-muted-foreground"> ({v.refunds} refunded)</span>}</> },
              { title: "Conversion", num: true, sort: (v) => v.conversion, cell: (v) => pct(v.conversion, 2) },
              { title: "Revenue", num: true, sort: (v) => v.net_usd_micros, cell: (v) => <span title={`gross ${usd(v.gross_usd_micros)}, ${v.renewals} renewal(s)`}>{usd(v.net_usd_micros)}</span> },
              { title: "ARPU / view", num: true, sort: (v) => v.arpu_per_view_usd_micros, cell: (v) => (v.arpu_per_view_usd_micros === null ? "—" : usd(v.arpu_per_view_usd_micros)) },
              {
                title: "Probability to be best",
                sort: (v) => v.probability_best,
                size: 200,
                className: "w-48",
                cell: (v) => (
                  <div className="flex items-center gap-2">
                    <div className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-muted">
                      <div className={v.probability_best === bestP && r.variants.length > 1 ? "h-full bg-emerald-500" : "h-full bg-muted-foreground/40"} style={{ width: `${(v.probability_best * 100).toFixed(1)}%` }} />
                    </div>
                    <span className="w-12 shrink-0 text-right text-xs tabular-nums">{pct(v.probability_best)}</span>
                  </div>
                ),
              },
            ]}
          />
          <p className="text-xs text-muted-foreground">
            Total: {r.totals.views.toLocaleString()} views, {r.totals.purchases.toLocaleString()} purchases, {usd(r.totals.net_usd_micros)} net.
            {r.unknown_variant_purchases > 0 && ` ${r.unknown_variant_purchases} purchase(s) name a variant that is not in this list.`}
            {r.fx.missing.length > 0 && ` No exchange rate for ${r.fx.missing.join(", ")}: left out of USD.`}
            {" "}Wait for a few hundred views per variant before you pick a winner.
          </p>
        </div>
      )}
    </Section>
  )
}
