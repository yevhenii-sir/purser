import { useEffect, useState } from "react"
import { CornerDownLeftIcon, UserSearchIcon } from "lucide-react"
import { Command, CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList, CommandShortcut } from "@/components/ui/command"
import type { AppGroup } from "@/lib/apps"
import { openPlayer } from "@/lib/player"
import { APP_TABS, appRoute, PAGES } from "@/routes"

const TAB_TITLES: Record<string, string> = {
  setup: "Setup", products: "Products", delivery: "Delivery", integrations: "Integrations", keys: "Keys", purchases: "Purchases", revenue: "Revenue",
}

/** Every word of the query appears in the item, in any order ("rings17 keys" finds "puzzle-17 → Keys"). */
function matchWords(value: string, search: string): number {
  const hay = value.toLowerCase()
  const words = search.toLowerCase().split(/\s+/).filter(Boolean)
  if (!words.length) return 1
  if (!words.every((w) => hay.includes(w))) return 0
  // Prefer matches at the start of the app name.
  return hay.startsWith(words[0]) ? 1 : 0.5
}

/** Ctrl/⌘ K: jump to any app (and any of its tabs) or page. */
export function CommandMenu({ open, setOpen, groups, go }: {
  open: boolean
  setOpen: (open: boolean) => void
  groups: AppGroup[]
  go: (route: string) => void
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault()
        setOpen(!open)
      }
    }
    document.addEventListener("keydown", onKey)
    return () => document.removeEventListener("keydown", onKey)
  }, [open, setOpen])

  const [query, setQuery] = useState("")
  const run = (route: string) => {
    setOpen(false)
    setQuery("")
    go(route)
  }

  return (
    <CommandDialog open={open} onOpenChange={setOpen} title="Go to" description="Search apps, their tabs and pages">
      <Command filter={matchWords}>
      <CommandInput placeholder="App, page, user_id or transaction id…" value={query} onValueChange={setQuery} />
      <CommandList>
        <CommandEmpty>Nothing found.</CommandEmpty>
        <CommandGroup heading="Apps">
          {groups.map((g) => (
            <CommandItem key={g.key} value={`${g.name} ${g.key} ${g.apps.map((a) => a.app_id).join(" ")}`} onSelect={() => run(appRoute(g.key))}>
              <span className="truncate">{g.name}</span>
              <span className="ml-2 truncate font-mono text-xs text-muted-foreground">{g.apps.map((a) => a.app_id).join(" · ")}</span>
              <CommandShortcut><CornerDownLeftIcon className="size-3" /></CommandShortcut>
            </CommandItem>
          ))}
        </CommandGroup>
        {query.trim() && <CommandGroup heading="App tabs">
          {groups.flatMap((g) =>
            APP_TABS.filter((t) => t !== "setup").map((t) => (
              <CommandItem key={`${g.key}/${t}`} value={`${g.name} ${g.key} ${TAB_TITLES[t]} ${g.apps.map((a) => a.app_id).join(" ")}`} onSelect={() => run(appRoute(g.key, t))}>
                <span className="truncate">{g.name}</span>
                <span className="text-muted-foreground">→ {TAB_TITLES[t]}</span>
              </CommandItem>
            )),
          )}
        </CommandGroup>}
        {query.trim() && <CommandGroup heading="Players">
          <CommandItem value={`find player transaction ${query}`} onSelect={() => { const q = query; setOpen(false); setQuery(""); void openPlayer(q, go) }}>
            <UserSearchIcon />
            <span className="shrink-0">Find player / transaction</span>
            <span className="truncate font-mono text-xs text-muted-foreground">{query.trim()}</span>
          </CommandItem>
        </CommandGroup>}
        <CommandGroup heading="Pages">
          {Object.entries(PAGES).map(([key, p]) => (
            <CommandItem key={key} value={`page ${p.title}`} onSelect={() => run(key)}>
              <p.icon />
              {p.title}
            </CommandItem>
          ))}
        </CommandGroup>
      </CommandList>
      </Command>
    </CommandDialog>
  )
}
