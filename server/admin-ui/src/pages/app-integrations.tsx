import { useState } from "react"
import { MegaphoneIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Badge } from "@/components/reui/badge"
import { IconTile } from "@/components/reui/icon-tile"
import { IntegrationDialog, TestEvent } from "@/components/integrations"
import { KIND_LIST, KINDS, type Integration, type Kind } from "@/components/integration-kinds"
import { ago } from "@/lib/format"
import { buildLabel, platformById } from "@/lib/stores"
import type { GameApp, IntegrationHealth } from "@/lib/types"
import type { TabProps } from "@/pages/app"

/** Integrations keyed by mobile device ids: the server skips builds without a mobile OS. */
const MOBILE_ONLY: Kind[] = ["ga4", "appsflyer", "adjust", "tenjin", "appmetrica"]

type BuildState = "off" | "enabled" | "disabled" | "no_key" | "not_mobile"
type ShownState = Exclude<BuildState, "not_mobile">

function StatusBadge({ state }: { state: ShownState }) {
  if (state === "off") return <span className="text-xs text-muted-foreground">Not connected</span>
  const [variant, label] = {
    enabled: ["success-light", "Enabled"],
    disabled: ["secondary", "Disabled"],
    no_key: ["destructive-light", "Key missing"],
  }[state] as ["success-light" | "secondary" | "destructive-light", string]
  return (
    <Badge variant={variant} radius="full">
      <span className="size-1.5 rounded-full bg-current" />
      {label}
    </Badge>
  )
}

type Activity = { tone: "ok" | "warn" | "fail"; text: string; title: string }

/** The last thing that happened to the events of an enabled integration (the same rule as the
 * setup check: a failure or a skip after the last sent event wins). */
function activityOf(h: IntegrationHealth | undefined): Activity | null {
  if (!h) return null
  const sent = Number(h.last_sent_at_micros)
  const failed = Number(h.last_failed_at_micros)
  const skipped = Number(h.last_skipped_at_micros)
  if (failed > 0 && failed >= Math.max(sent, skipped)) {
    return h.last_failure_final
      ? { tone: "fail", text: `Event dropped ${ago(failed)}`, title: h.last_error }
      : { tone: "warn", text: `Failed ${ago(failed)}, retrying`, title: h.last_error }
  }
  if (skipped > sent) return { tone: "warn", text: `Skipped ${ago(skipped)}: no device id`, title: h.last_skip_reason }
  if (sent > 0) return { tone: "ok", text: `Sent ${ago(sent)}`, title: "Last event accepted by the service" }
  return null
}

const TONE = { ok: "text-muted-foreground", warn: "text-amber-600 dark:text-amber-400", fail: "text-destructive" }

export function AppIntegrationsTab({ data, reload }: TabProps) {
  const g = data.group
  const [editing, setEditing] = useState<{ kind: Kind; row: Integration | null; appId: string } | null>(null)
  const keySet = (kind: Kind, appId: string) => {
    const app = g.apps.find((a) => a.app_id === appId)
    return [appId, app?.account || "", "default"].some((s) => data.creds.has(`${s}/${KINDS[kind].secret}`))
  }
  const stateOf = (kind: Kind, a: GameApp): { state: BuildState; row?: Integration } => {
    const row = data.integrations.find((i) => i.app_id === a.app_id && i.kind === kind) as Integration | undefined
    if (!row) return { state: MOBILE_ONLY.includes(kind) && !platformById(a.platform)?.os ? "not_mobile" : "off" }
    if (!row.enabled) return { state: "disabled", row }
    return { state: keySet(kind, a.app_id) ? "enabled" : "no_key", row }
  }
  return (
    <>
      <p className="text-sm text-muted-foreground">Purchase events of this app go to every enabled integration, separately for each build.</p>
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        {KIND_LIST.map((kind) => {
          const def = KINDS[kind]
          const all = g.apps.map((a) => ({ app: a, ...stateOf(kind, a) }))
          const states = all.filter((s): s is typeof s & { state: ShownState } => s.state !== "not_mobile")
          const skipped = all.filter((s) => s.state === "not_mobile").map((s) => buildLabel(s.app))
          const on = states.filter((s) => s.state === "enabled").length
          const problems = states.filter((s) => s.state === "no_key").length
          const health = (appId: string) => data.health.find((h) => h.app_id === appId && h.kind === kind)
          const failing = states.filter((s) => s.state === "enabled" && activityOf(health(s.app.app_id))?.tone === "fail").length
          return (
            <Card key={kind} className="gap-0 overflow-hidden py-0">
              <div className="flex items-start gap-3 p-4">
                <IconTile variant="frame" size="sm"><def.icon /></IconTile>
                <div className="min-w-0 flex-1">
                  <div className="flex items-center justify-between gap-2">
                    <div className="truncate font-medium">{def.name}</div>
                    {problems > 0 ? (
                      <Badge variant="destructive-light" size="sm" radius="full">Key missing</Badge>
                    ) : failing > 0 ? (
                      <Badge variant="destructive-light" size="sm" radius="full">Failing</Badge>
                    ) : on > 0 ? (
                      <Badge variant="success-light" size="sm" radius="full">{on === states.length ? "On" : `${on} of ${states.length} on`}</Badge>
                    ) : null}
                  </div>
                  <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{def.description}</p>
                </div>
              </div>
              <div className="flex-1 divide-y border-t bg-muted/30">
                {states.map(({ app, state, row }) => {
                  const act = state === "enabled" ? activityOf(health(app.app_id)) : null
                  return (
                  <div key={app.app_id} className="flex min-h-11 items-center gap-2 px-4 py-1.5">
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm" title={app.app_id}>{buildLabel(app)}</div>
                      {act && <div className={`truncate text-xs ${TONE[act.tone]}`} title={act.title}>{act.text}</div>}
                    </div>
                    <StatusBadge state={state} />
                    <Button size="xs" variant={row ? "ghost" : "outline"} onClick={() => setEditing({ kind, row: row || null, appId: app.app_id })}>
                      {row ? "Edit" : "Connect"}
                    </Button>
                  </div>
                  )
                })}
                {skipped.length > 0 && (
                  <p className="px-4 py-2 text-xs text-muted-foreground" title="Events are matched to installs by mobile device ids">
                    Mobile builds only: not for {skipped.join(", ")}
                  </p>
                )}
              </div>
            </Card>
          )
        })}
      </div>
      {g.ios && (
        <Card className="gap-3">
          <CardHeader>
            <div className="flex items-center gap-3">
              <IconTile variant="frame" size="sm"><MegaphoneIcon /></IconTile>
              <div className="min-w-0">
                <CardTitle className="text-base">Apple Ads</CardTitle>
                <CardDescription>Attribution only: Apple takes no purchase events from servers.</CardDescription>
              </div>
            </div>
          </CardHeader>
          <CardContent className="space-y-2 text-sm text-muted-foreground">
            <p>
              Turn on <span className="font-medium text-foreground">Collect Apple Ads attribution</span> in the Unity <code>BillingConfig</code>.
              On the first launch the SDK takes the AdServices token on the device, asks Apple which campaign brought the install and sends
              <code> asa_campaign_id</code>, <code>asa_ad_group_id</code>, <code>asa_keyword_id</code> with purchases.
            </p>
            <p>The ids go to the webhook and Cloud Storage exports, where revenue can be grouped by campaign. Adjust, AppsFlyer and Tenjin attribute Apple Ads installs through their own SDKs.</p>
          </CardContent>
        </Card>
      )}
      <TestEvent apps={g.apps} />
      <IntegrationDialog
        editing={editing && { kind: editing.kind, row: editing.row }}
        apps={editing ? g.apps.filter((a) => a.app_id === editing.appId) : g.apps}
        onClose={() => setEditing(null)}
        onSaved={reload}
      />
    </>
  )
}
