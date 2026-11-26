// Moving to another server (docs/moving-servers.md): on the old server the pause and the forwarding
// with what still reaches it; on the new one what still points at the old address.
import { useMemo, useState } from "react"
import { ArrowRightIcon, PauseIcon, TriangleAlertIcon } from "lucide-react"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { ErrorBox, Loading, Mono, Section, Tone } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { getSetting, proc, select } from "@/lib/api"
import { useAppIndex } from "@/lib/apps"
import { ago } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { useAllowed } from "@/lib/roles"
import { buildLabel, storeById } from "@/lib/stores"

interface Move {
  mode: "paused" | "forwarding"
  target: string
  since_micros: number
}

interface ForwardedRow {
  day: number
  route: string
  app_id: string
  requests: number
  failed: number
  last_at_micros: number
}

interface Checklist {
  moved_in: { from: string; at_micros: number } | null
  builds?: { app_id: string; game_key: string; store: string; idc: boolean; verified: boolean; notified: boolean | null }[]
  sign_in?: boolean
  identity?: string
  token_admins?: { identity: string; name: string }[]
}

const DOCS = "https://yevhenii-sir.github.io/purser/moving-servers/"

const parse = <T,>(v: string): T | null => {
  try {
    return v ? (JSON.parse(v) as T) : null
  } catch {
    return null
  }
}

async function moveState(): Promise<Move | null> {
  return parse<Move>(await getSetting("move"))
}

/** Above every page while this server is moving: what it does with the routes, and that changes here are not copied. */
export function MovingBanner() {
  const { data } = useAsync(moveState)
  if (!data) return null
  return (
    <Alert variant="destructive">
      <TriangleAlertIcon />
      <AlertTitle>{data.mode === "paused" ? "This server is paused for a move" : "This server forwards to another one"}</AlertTitle>
      <AlertDescription>
        {data.mode === "paused"
          ? "Games and stores get 503 and try again; background jobs wait. Copy the database, then forward this server (Settings → Server)."
          : <>Every route goes on to <Mono className="break-all">{data.target}</Mono>. Changes made on this page are not copied to the new server: make them there.</>}
      </AlertDescription>
    </Alert>
  )
}

/** Server page: the move of this server, or what still points at the old one after a move here. */
export function MovingSection() {
  const owner = useAllowed("owner")
  const state = useAsync(async () => ({ move: await moveState(), checklist: await proc<Checklist>("admin_move_checklist") }))
  const [target, setTarget] = useState("")
  const [busy, setBusy] = useState(false)
  const set = async (mode: string, to: string, ok: string) => {
    setBusy(true)
    const done = await tryAct(() => proc("admin_set_move", mode, to.trim()), ok)
    setBusy(false)
    if (done !== undefined) state.reload()
  }
  if (state.error) return <Section title="Moving to another server"><ErrorBox error={state.error} /></Section>
  if (state.loading || !state.data) return <Section title="Moving to another server"><Loading rows={2} /></Section>
  const { move, checklist } = state.data
  return (
    <>
      {checklist.moved_in && <MovedInSection checklist={checklist} onDone={state.reload} />}
      <Section
        title="Moving to another server"
        description={<>Usually <code>python migrate.py … --switch</code> does every step: pauses this server, copies the database, then forwards here to the new one (<a className="underline" href={DOCS} target="_blank" rel="noreferrer">moving to another server</a>).</>}
      >
        {!move ? (
          <div className="space-y-3 text-sm">
            <p className="text-muted-foreground">Not moving: this server answers its routes itself.</p>
            {owner && (
              <Button variant="outline" size="sm" disabled={busy} onClick={() => {
                if (confirm("Pause this server? Games and stores get 503 and try again later, background jobs wait. Copy the database, then forward this server to the new one.")) void set("paused", "", "Paused")
              }}><PauseIcon /> Pause for a move</Button>
            )}
          </div>
        ) : (
          <div className="space-y-4 text-sm">
            <div className="flex flex-wrap items-center gap-2">
              <Tone tone="warn">{move.mode}</Tone>
              {move.mode === "forwarding" && <>to <Mono className="break-all">{move.target}</Mono></>}
              <span className="text-xs text-muted-foreground">since {ago(move.since_micros)}</span>
            </div>
            {owner && (
              <div className="flex flex-wrap gap-2">
                <Input className="h-8 max-w-md font-mono text-xs" placeholder="https://<host>/v1/database/<new-db>/route" value={target || (move.mode === "forwarding" ? move.target : "")}
                  onChange={(e) => setTarget(e.target.value)} />
                <Button size="sm" disabled={busy || !(target || move.target).trim()} onClick={() => set("forwarding", target || move.target, "Forwarding to the new server")}>
                  <ArrowRightIcon /> Forward
                </Button>
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => {
                  if (confirm("End the move? This server answers its routes itself again, and its background jobs run.")) void set("", "", "This server serves its routes again")
                }}>End the move</Button>
              </div>
            )}
            {move.mode === "forwarding" && <ForwardedCounts />}
          </div>
        )}
      </Section>
    </>
  )
}

/** What still reaches the forwarding server, per route and build, over the last 14 days. */
function ForwardedCounts() {
  const [today] = useState(() => Math.floor(Date.now() / 86_400_000))
  const { data, error, loading } = useAsync(() => select<ForwardedRow>({ table: "forwarded_daily", where: { column: "since", equals: String(today - 13) } }))
  const { builds, groupOf } = useAppIndex()
  const rows = useMemo(() => {
    const out = new Map<string, { route: string; app_id: string; requests: number; failed: number; last: number; today: number }>()
    for (const r of data || []) {
      const k = `${r.route}|${r.app_id}`
      const o = out.get(k) || { route: r.route, app_id: r.app_id, requests: 0, failed: 0, last: 0, today: 0 }
      o.requests += Number(r.requests)
      o.failed += Number(r.failed)
      o.last = Math.max(o.last, Number(r.last_at_micros))
      if (Number(r.day) === today) o.today += Number(r.requests)
      out.set(k, o)
    }
    return [...out.values()]
  }, [data, today])
  if (error) return <ErrorBox error={error} />
  if (loading || !data) return <Loading rows={2} />
  const label = (appId: string) => {
    const b = builds.get(appId)
    return b ? `${groupOf.get(appId)?.name ?? b.game_key} · ${buildLabel(b)}` : appId || "—"
  }
  return (
    <div className="space-y-2">
      <p className="text-xs text-muted-foreground">
        What still reaches this server, last 14 days. When nothing arrives for a while, every game and store uses the new address: delete this database.
      </p>
      <RichTable
        rows={rows}
        rowKey={(r) => `${r.route}|${r.app_id}`}
        noun="routes"
        pageSize={10}
        initialSort={{ id: "Last", desc: true }}
        empty="Nothing forwarded yet."
        columns={[
          { title: "Route", size: 180, sort: (r) => r.route, cell: (r) => <Mono>{r.route}</Mono> },
          { title: "Build", size: 260, sort: (r) => label(r.app_id), cell: (r) => <span className="block truncate" title={r.app_id}>{label(r.app_id)}</span> },
          { title: "Today", size: 90, num: true, sort: (r) => r.today, cell: (r) => r.today },
          { title: "14 days", size: 90, num: true, sort: (r) => r.requests, cell: (r) => r.requests },
          { title: "Failed", size: 90, num: true, sort: (r) => r.failed, cell: (r) => (r.failed ? <Tone tone="bad">{r.failed}</Tone> : 0) },
          { title: "Last", size: 140, sort: (r) => r.last, cell: (r) => <span className="text-xs">{ago(r.last)}</span> },
        ]}
      />
    </div>
  )
}

/** On the new server: what still points at the old one, per build and overall. */
function MovedInSection({ checklist, onDone }: { checklist: Checklist; onDone: () => void }) {
  const owner = useAllowed("owner")
  const { groupOf } = useAppIndex()
  const builds = checklist.builds || []
  const clients = builds.filter((b) => b.verified).length
  const notifying = builds.filter((b) => b.notified !== null)
  const notified = notifying.filter((b) => b.notified).length
  const idc = builds.filter((b) => b.idc)
  const admin = location.origin + location.pathname
  return (
    <Section
      title="Moved in"
      description={<>Copied from <Mono>{checklist.moved_in!.from}</Mono> {ago(checklist.moved_in!.at_micros)}. Each line turns green when it reaches this server.</>}
      actions={owner && (
        <Button size="sm" variant="outline" onClick={async () => {
          if (await tryAct(() => proc("admin_mark_moved_in", ""), "Done") !== undefined) onDone()
        }}>Done</Button>
      )}
    >
      <div className="space-y-4">
        <div className="flex flex-wrap gap-3 text-sm">
          <Tone tone={clients === builds.length ? "ok" : "warn"}>clients {clients} of {builds.length} builds</Tone>
          <Tone tone={notified === notifying.length ? "ok" : "warn"}>store notifications {notified} of {notifying.length}</Tone>
        </div>
        <RichTable
          rows={builds}
          rowKey={(b) => b.app_id}
          noun="builds"
          pageSize={10}
          initialSort={{ id: "Waiting", desc: true }}
          columns={[
            { title: "App", size: 200, sort: (b) => groupOf.get(b.app_id)?.name ?? b.game_key, cell: (b) => groupOf.get(b.app_id)?.name ?? b.game_key },
            { title: "Build", size: 240, sort: (b) => b.app_id, cell: (b) => <span className="flex flex-col"><span>{storeById(b.store)?.name ?? b.store}</span><Mono className="text-muted-foreground">{b.app_id}</Mono></span> },
            { title: "Clients", size: 110, sort: (b) => Number(b.verified), cell: (b) => (b.verified ? <Tone tone="ok">here</Tone> : <Tone tone="muted">not yet</Tone>) },
            {
              title: "Store notifications",
              size: 160,
              sort: (b) => (b.notified === null ? 2 : Number(b.notified)),
              cell: (b) => (b.notified === null ? <span className="text-muted-foreground">none</span> : b.notified ? <Tone tone="ok">here</Tone> : <Tone tone="warn">not yet</Tone>),
            },
            { id: "Waiting", title: "", size: 1, hidden: true, sort: (b) => Number(!b.verified) + Number(b.notified === false), cell: () => null },
          ]}
        />
        <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
          <li>Store notifications that are not here yet: change their URL in the store console, or press Set up on the build's Setup tab (App Store, Stripe, Telegram, Google Cloud).</li>
          {checklist.sign_in && <li>Sign-in through a provider: add <Mono className="break-all">{admin}</Mono> as a redirect URI of its client.</li>}
          {idc.length > 0 && (
            <li>
              {idc.length} build{idc.length === 1 ? "" : "s"} deliver{idc.length === 1 ? "s" : ""} by inter-database calls: allow this database's identity
              <Mono className="mx-1 break-all">{checklist.identity}</Mono>in the game database, which must be on this server too.
            </li>
          )}
          {(checklist.token_admins?.length ?? 0) > 0 && (
            <li>Admins who sign in with tokens ({checklist.token_admins!.map((a) => a.name || a.identity.slice(0, 10) + "…").join(", ")}): tokens of the old server may not work here; give them new tokens or invite them by email.</li>
          )}
        </ul>
      </div>
    </Section>
  )
}
