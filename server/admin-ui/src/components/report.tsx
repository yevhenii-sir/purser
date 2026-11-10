import { useState } from "react"
import { KeyRoundIcon, RefreshCwIcon, WifiOffIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { CopyField, DataTable, DialogBody, ErrorBox, LevelBadge, Loading, Mono } from "@/components/common"
import { call, genSecret, proc, publicRoute } from "@/lib/api"
import { checkOptions } from "@/lib/checks"
import { fmtMs } from "@/lib/format"
import { tryAct, useAsync } from "@/lib/hooks"
import { storeById } from "@/lib/stores"
import type { Report } from "@/lib/types"

/** Notification URLs of a report with the route base filled in and known secrets substituted.
 * `notify` is the build's store notification URL (any store), `notifyLabel` where it goes. */
export function reportUrls(rep: Report, base: string, secrets: Record<string, string> = {}) {
  const fill = (u?: string) =>
    u ? (u.startsWith("/") ? base + u : u).replace(/<(\w+)>/g, (m, k: string) => secrets[k] || m) : ""
  const store = storeById(rep.urls.store || "")
  const notify = fill(rep.urls.notify)
  return {
    notify,
    /** Google: the RTDN URL, whose secret the page can replace (`New RTDN secret`). */
    rtdn: store?.id === "google" ? notify : "",
    notifyLabel: rep.urls.notify_label || store?.notify_label || "Store notifications URL",
    notifySecret: store?.notify_secret || "",
    identity: rep.urls.billing_identity || "",
  }
}

export function ReportView({ report }: { report: Report }) {
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <LevelBadge level={report.summary} />
        <Mono className="font-medium">{report.app_id}</Mono>
        <span className="text-muted-foreground">checked {fmtMs(report.checked_at_ms)}</span>
      </div>
      <DataTable
        rows={report.checks}
        columns={[
          { title: "", cell: (c) => <LevelBadge level={c.level} />, className: "w-16" },
          { title: "Check", cell: (c) => <span className="font-medium whitespace-nowrap">{c.title}</span> },
          {
            title: "Result",
            cell: (c) => (
              <div className="space-y-1 whitespace-normal">
                <div className="break-words">{c.detail}</div>
                {c.fix && (c.level === "warn" || c.level === "fail") && <div className="text-xs text-muted-foreground">{c.fix}</div>}
              </div>
            ),
          },
        ]}
      />
    </div>
  )
}

export function CheckDialog({ appId, onClose }: { appId: string | null; onClose: () => void }) {
  const [secrets, setSecrets] = useState<Record<string, string>>({})
  const [offline, setOffline] = useState(false)
  const { data, error, loading, reload } = useAsync(async () => {
    if (!appId) return null
    const [report, base] = await Promise.all([proc<Report>("admin_check_app", appId, offline ? JSON.stringify({ online: false, apple_test: false }) : checkOptions()), publicRoute()])
    return { report, base }
  }, [appId, offline])
  const urls = data ? reportUrls(data.report, data.base, secrets) : null

  const newRtdnSecret = async () => {
    if (!appId || !confirm("Replace the RTDN secret of this app? Notifications fail until the Pub/Sub push URL is updated.")) return
    const secret = genSecret()
    const ok = await tryAct(() => call("admin_set_credential", appId, "google_rtdn_secret", secret), "New secret saved: copy the URL now")
    if (ok !== undefined) {
      setSecrets({ google_rtdn_secret: secret })
      reload()
    }
  }

  return (
    <Dialog open={!!appId} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Setup check</DialogTitle>
          <DialogDescription>Configuration, store access, notifications and the delivery target, with a fix for every problem.</DialogDescription>
        </DialogHeader>
        <DialogBody>
          {error ? (
            <ErrorBox error={error} />
          ) : loading || !data || !urls ? (
            <div className="space-y-3">
              <p className="text-sm text-muted-foreground">Checking… (up to 10 seconds)</p>
              <Loading rows={5} />
            </div>
          ) : (
            <div className="space-y-5">
              <ReportView report={data.report} />
              <div className="space-y-3">
                {urls.notify && <CopyField label={urls.notifyLabel} value={urls.notify} />}
                {urls.rtdn.includes("<google_rtdn_secret>") && (
                  <p className="text-xs text-muted-foreground">The stored secret is never shown. Generate a new one to get the full URL, then update the Pub/Sub push subscription.</p>
                )}
                <CopyField label="Billing identity (the game must trust it)" value={urls.identity} />
              </div>
            </div>
          )}
        </DialogBody>
        <DialogFooter className="gap-2 sm:justify-between">
          <Button variant="ghost" onClick={() => setOffline((o) => !o)}>
            <WifiOffIcon /> {offline ? "Full check" : "Offline check only"}
          </Button>
          <div className="flex flex-wrap gap-2">
            {urls?.rtdn && <Button variant="outline" onClick={newRtdnSecret}><KeyRoundIcon /> New RTDN secret</Button>}
            <Button onClick={reload}><RefreshCwIcon /> Check again</Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
