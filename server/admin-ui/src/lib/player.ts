import { toast } from "sonner"
import { proc } from "@/lib/api"
import { errorText } from "@/lib/format"
import { playerRoute } from "@/routes"

interface Match {
  user_id: string
  /** `user_id` | `attribution` | `transaction` | `original_transaction` | `purchase_token` */
  via: string
  purchase_id: string
}

/**
 * Opens the player a query points to: a user_id, a transaction / order id (GPA.…), a purchase id
 * or a Google purchase token. Unknown queries open as a user_id (for a manual grant to a new player).
 */
export async function openPlayer(query: string, go: (route: string) => void) {
  const q = query.trim()
  if (!q) return
  try {
    const { matches } = await proc<{ matches: Match[] }>("admin_find_player", q)
    if (!matches.length) {
      toast.info("No purchase matches: opened as a user_id")
      return go(playerRoute(q))
    }
    if (matches.length > 1) toast.info(`Other players with this id: ${matches.slice(1).map((m) => m.user_id).join(", ")}`)
    go(playerRoute(matches[0].user_id))
  } catch (e) {
    toast.error(errorText(e))
  }
}
