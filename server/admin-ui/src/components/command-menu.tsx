import { useEffect, useMemo, useState } from "react"
import { CornerDownLeftIcon, UserSearchIcon } from "lucide-react"
import { Command, CommandDialog, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList, CommandShortcut } from "@/components/ui/command"
import { appSearchText, matchWords, type AppGroup } from "@/lib/apps"
import { openPlayer } from "@/lib/player"
import { APP_TABS, appRoute, PAGES } from "@/routes"

const TAB_TITLES: Record<string, string> = {
  setup: "Setup", products: "Products", delivery: "Delivery", integrations: "Integrations", keys: "Keys", purchases: "Purchases", revenue: "Revenue",
}

/** Apps whose tabs are offered for a query: the best matches only, not every app × every tab. */
const TAB_APPS = 8
const TAB_WORDS = Object.values(TAB_TITLES).map((t) => t.toLowerCase())

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
  // Search text of every app, once per app list.
  const indexed = useMemo(() => groups.map((g) => ({ g, text: appSearchText(g) })), [groups])
  // Tab entries for the top apps by name / key / bundle id; a word that starts a tab's name
  // ("key", "purch") does not have to match the app.
  const tabApps = useMemo(() => {
    const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean)
    if (!words.length) return []
    const tabWord = (w: string) => TAB_WORDS.some((t) => t.startsWith(w))
    // Only tab words ("keys"): the first apps by name.
    if (words.every(tabWord)) return indexed.slice(0, TAB_APPS).map((x) => x.g)
    const scored = indexed.flatMap(({ g, text }) => {
      if (!words.every((w) => text.includes(w) || tabWord(w))) return []
      const first = words.find((w) => text.includes(w))
      return first ? [{ g, score: text.startsWith(first) ? 1 : 0.5 }] : []
    })
    // A stable sort keeps name order among equal scores.
    return scored.sort((a, b) => b.score - a.score).slice(0, TAB_APPS).map((x) => x.g)
  }, [indexed, query])
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
          {indexed.map(({ g, text }) => (
            <CommandItem key={g.key} value={text} onSelect={() => run(appRoute(g.key))}>
              <span className="truncate">{g.name}</span>
              <span className="ml-2 truncate font-mono text-xs text-muted-foreground">{g.apps.map((a) => a.app_id).join(" · ")}</span>
              <CommandShortcut><CornerDownLeftIcon className="size-3" /></CommandShortcut>
            </CommandItem>
          ))}
        </CommandGroup>
        {query.trim() && <CommandGroup heading="App tabs">
          {tabApps.flatMap((g) =>
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
