import { proc } from "./api"
import { errorText } from "./format"
import { storeById } from "./stores"
import type { Report } from "./types"

/** Options of `admin_check_app`. `online: false` checks the configuration only (no store calls).
 * The Apple test notification is opt-in: it asks Apple to send a TEST notification to the server on every check. */
export const checkOptions = ({ online = true, appleTest = false } = {}) => JSON.stringify({ online, apple_test: appleTest })

/** Setup check of several builds at once. A failed call becomes a report with one failed check. */
export function checkBuilds(appIds: string[], appleTest = false): Promise<Report[]> {
  return Promise.all(
    appIds.map((id) =>
      proc<Report>("admin_check_app", id, checkOptions({ appleTest })).catch((e): Report => ({
        app_id: id, summary: "fail", checked_at_ms: Date.now(), urls: {},
        checks: [{ id: "error", title: "Check", level: "fail", detail: errorText(e), fix: "" }],
      })),
    ),
  )
}

/** Notification URLs of a report with the route base filled in and known secrets substituted.
 * `notify` is the build's store notification URL (any store), `notifyLabel` where it goes. */
export function reportUrls(rep: Report, base: string, secrets: Record<string, string> = {}) {
  const fill = (u?: string) =>
    u ? (u.startsWith("/") ? base + u : u).replace(/<(\w+)>/g, (m, k: string) => secrets[k] || m) : ""
  const store = storeById(rep.urls.store || "")
  const notify = fill(rep.urls.notify)
  return {
    notify,
    notifyLabel: rep.urls.notify_label || store?.notify_label || "Store notifications URL",
    notifySecret: store?.notify_secret || "",
    identity: rep.urls.billing_identity || "",
    /** Google: the push subscription authenticates as this service account, for this audience. */
    pushAccount: rep.urls.push_service_account || "",
    pushAudience: rep.urls.push_audience || "",
  }
}
