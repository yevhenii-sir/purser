import { useState, type ReactNode } from "react"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Mono } from "@/components/common"
import { appSearchText, matchWords, useApps } from "@/lib/apps"

/**
 * Ticks apps (`game_key`s) by display name, with a search box for long lists. What an empty choice
 * means is the caller's (usually "every app"): `summary` says it above the list.
 */
export function AppsPicker({ value, onChange, summary }: { value: string[]; onChange: (keys: string[]) => void; summary?: ReactNode }) {
  const { groups } = useApps()
  const [query, setQuery] = useState("")
  const shown = (groups || []).filter((g) => matchWords(appSearchText(g), query) > 0)
  const toggle = (key: string, on: boolean) => onChange(on ? [...value, key] : value.filter((k) => k !== key))
  return (
    <div className="space-y-2">
      {summary && <p className="text-xs text-muted-foreground">{summary}</p>}
      {(groups || []).length > 8 && <Input className="h-8" placeholder="Filter apps" value={query} onChange={(e) => setQuery(e.target.value)} />}
      <div className="max-h-56 space-y-1 overflow-y-auto rounded-md border p-2">
        {shown.map((g) => (
          <label key={g.key} className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 text-sm hover:bg-muted">
            <Checkbox className="shrink-0" checked={value.includes(g.key)} onCheckedChange={(v) => toggle(g.key, v === true)} />
            <span className="min-w-0 truncate">{g.name}</span>
            <Mono className="ms-auto shrink-0 text-muted-foreground">{g.key}</Mono>
          </label>
        ))}
        {!shown.length && <p className="px-1 text-sm text-muted-foreground">{groups?.length ? "No app matches." : "No apps yet."}</p>}
      </div>
    </div>
  )
}
