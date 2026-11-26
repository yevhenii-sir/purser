import { useMemo, useState, type ReactNode } from "react"
import { XIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Mono } from "@/components/common"
import { appSearchText, appsText, matchWords, useApps } from "@/lib/apps"

/** Apps listed at once: a server with hundreds narrows the list with the filter. */
const MAX_LISTED = 100
/** Chosen apps named above the list; the rest are counted. */
const MAX_CHIPS = 12

/** The apps (`game_key`s) of a row in one line, all of them in the tooltip. */
export function AppsSummary({ keys, none = "Every app", className }: { keys: string[]; none?: string; className?: string }) {
  const { groups } = useApps()
  const names = useMemo(() => {
    const byKey = new Map((groups || []).map((g) => [g.key, g.name]))
    return keys.map((k) => byKey.get(k) || k)
  }, [groups, keys])
  return (
    <span className={className ?? "text-sm text-muted-foreground"} title={names.length > 1 ? names.join("\n") : undefined}>
      {appsText(names, none)}
    </span>
  )
}

/**
 * Ticks apps (`game_key`s) by display name. The chosen ones are named above the list (and come first
 * in it); a filter narrows long lists, and everything it matches can be ticked at once. What an empty
 * choice means is the caller's (usually "every app"): `summary` says it above the list.
 */
export function AppsPicker({ value, onChange, summary }: { value: string[]; onChange: (keys: string[]) => void; summary?: ReactNode }) {
  const { groups } = useApps()
  const [query, setQuery] = useState("")
  const all = groups || []
  const chosen = new Set(value)
  const names = new Map(all.map((g) => [g.key, g.name]))
  const matches = all.filter((g) => matchWords(appSearchText(g), query) > 0)
  // Without a filter the chosen apps come first: with hundreds of apps they would be lost in the list.
  const ordered = query.trim() ? matches : [...matches.filter((g) => chosen.has(g.key)), ...matches.filter((g) => !chosen.has(g.key))]
  const listed = ordered.slice(0, MAX_LISTED)
  const toggle = (key: string, on: boolean) => onChange(on ? [...value, key] : value.filter((k) => k !== key))
  const unticked = matches.filter((g) => !chosen.has(g.key))
  return (
    <div className="space-y-2">
      {summary && <p className="text-xs text-muted-foreground">{summary}</p>}
      {value.length > 0 && (
        <div className="flex flex-wrap items-center gap-1">
          {value.slice(0, MAX_CHIPS).map((k) => (
            <span key={k} className="inline-flex max-w-48 items-center gap-0.5 rounded-md border bg-muted/50 py-0.5 ps-2 pe-0.5 text-xs">
              <span className="truncate">{names.get(k) || k}</span>
              <button type="button" className="rounded p-0.5 text-muted-foreground hover:bg-muted hover:text-foreground" aria-label={`Remove ${names.get(k) || k}`} onClick={() => toggle(k, false)}>
                <XIcon className="size-3" />
              </button>
            </span>
          ))}
          {value.length > MAX_CHIPS && <span className="text-xs text-muted-foreground" title={value.slice(MAX_CHIPS).map((k) => names.get(k) || k).join("\n")}>+{value.length - MAX_CHIPS} more</span>}
          <Button variant="link" size="xs" className="h-auto px-1 text-muted-foreground" onClick={() => onChange([])}>Clear</Button>
        </div>
      )}
      {all.length > 8 && (
        <div className="flex gap-2">
          <Input className="h-8" placeholder={`Filter ${all.length} apps by name or key`} value={query} onChange={(e) => setQuery(e.target.value)} />
          {query.trim() && unticked.length > 0 && (
            <Button variant="outline" size="sm" className="h-8 shrink-0" onClick={() => onChange([...value, ...unticked.map((g) => g.key)])}>
              Tick {unticked.length === 1 ? "it" : `all ${unticked.length}`}
            </Button>
          )}
        </div>
      )}
      <div className="max-h-56 space-y-1 overflow-y-auto rounded-md border p-2">
        {listed.map((g) => (
          <label key={g.key} className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-sm hover:bg-muted">
            <Checkbox className="shrink-0" checked={chosen.has(g.key)} onCheckedChange={(v) => toggle(g.key, v === true)} />
            <span className="min-w-0 truncate">{g.name}</span>
            <Mono className="ms-auto shrink-0 text-muted-foreground">{g.key}</Mono>
          </label>
        ))}
        {!listed.length && <p className="px-1 text-sm text-muted-foreground">{all.length ? "No app matches." : "No apps yet."}</p>}
        {ordered.length > listed.length && (
          <p className="px-1 pt-1 text-xs text-muted-foreground">{listed.length} of {ordered.length} listed: type to narrow.</p>
        )}
      </div>
    </div>
  )
}
