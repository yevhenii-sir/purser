import { useEffect, useState } from "react"
import { RefreshCwIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { ago, errorText } from "@/lib/format"
import type { Kept } from "@/lib/types"
import { cn } from "@/lib/utils"
import { toast } from "sonner"

/** "updated 3 min ago" and a refresh button for a revenue report. Slow reports are kept on the
 * server and counted again at most once a minute: until then the button waits. */
export function RevenueUpdated({ stats, onRefresh, prefix, className }: {
  stats: Partial<Kept> | null | undefined
  onRefresh: () => unknown
  prefix?: string
  className?: string
}) {
  const [busy, setBusy] = useState(false)
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 5_000)
    return () => clearInterval(t)
  }, [])
  if (!stats?.computed_at_micros) return null
  const wait = Math.ceil((Number(stats.refresh_after_micros ?? 0) / 1000 - now) / 1000)
  const title = stats.counting ? "Another admin is counting it now" : wait > 0 ? `Can be counted again in ${wait} s` : "Count it again now"
  return (
    <span className={cn("inline-flex items-center gap-1", className)}>
      {prefix ? `${prefix} updated` : "updated"} {ago(stats.computed_at_micros)}
      <Button
        size="icon-xs"
        variant="ghost"
        title={title}
        aria-label={title}
        disabled={busy || wait > 0 || !!stats.counting}
        onClick={async (e) => {
          e.stopPropagation()
          setBusy(true)
          try {
            await onRefresh()
          } catch (err) {
            toast.error(errorText(err))
          } finally {
            setBusy(false)
            setNow(Date.now())
          }
        }}
      >
        <RefreshCwIcon className={cn(busy && "animate-spin")} />
      </Button>
    </span>
  )
}
