import { proc } from "./api"
import { errorText } from "./format"
import type { Report } from "./types"

/** Options of `admin_check_app`. The Apple test notification is opt-in: it asks Apple to send a
 * TEST notification to the server on every check. */
export const checkOptions = (appleTest = false) => JSON.stringify({ online: true, apple_test: appleTest })

/** Setup check of several builds at once. A failed call becomes a report with one failed check. */
export function checkBuilds(appIds: string[], appleTest = false): Promise<Report[]> {
  return Promise.all(
    appIds.map((id) =>
      proc<Report>("admin_check_app", id, checkOptions(appleTest)).catch((e): Report => ({
        app_id: id, summary: "fail", checked_at_ms: Date.now(), urls: {},
        checks: [{ id: "error", title: "Check", level: "fail", detail: errorText(e), fix: "" }],
      })),
    ),
  )
}
