import { call, genSecret, proc } from "./api"
import { errorText } from "./format"
import { tryAct } from "./hooks"
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

/** Replaces a Google Play build's RTDN secret (after asking) and re-checks it offline, so the stored
 * report has the new URL. Returns the new secret, the only time the page knows it; null if not replaced. */
export async function replaceRtdnSecret(appId: string): Promise<string | null> {
  if (!confirm("Replace the RTDN secret? Notifications fail until the Pub/Sub push URL is updated.")) return null
  const secret = genSecret()
  const ok = await tryAct(() => call("admin_set_credential", appId, "google_rtdn_secret", secret), "New secret saved: copy the URL now")
  if (ok === undefined) return null
  await tryAct(() => proc("admin_check_app", appId, checkOptions({ online: false })))
  return secret
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
    /** Google: the RTDN URL, whose secret the page can replace (`New RTDN secret`). */
    rtdn: store?.id === "google" ? notify : "",
    notifyLabel: rep.urls.notify_label || store?.notify_label || "Store notifications URL",
    notifySecret: store?.notify_secret || "",
    identity: rep.urls.billing_identity || "",
  }
}
