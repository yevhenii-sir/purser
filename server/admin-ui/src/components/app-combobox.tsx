import { useMemo, useState } from "react"
import { ChevronDownIcon, LayersIcon } from "lucide-react"
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { appSearchText, matchWords, useApps, type AppGroup } from "@/lib/apps"
import { buildLabel, PLATFORM_ICON } from "@/lib/stores"
import type { GameApp } from "@/lib/types"
import { cn } from "@/lib/utils"

/** cmdk item value of "All apps" (app keys and app ids never start with a colon). */
const ALL_VALUE = ":all"

/** cmdk filter: the item's value plus its keywords, matched word by word (`matchWords`). */
const filter = (value: string, search: string, keywords?: string[]) => matchWords(keywords?.length ? keywords.join(" ") : value, search)

/**
 * Searchable app picker for long app lists: apps by display name with their builds, found by name,
 * app key or bundle / app id. Keyboard: type to filter, arrows to move, Enter to pick, Esc to close.
 *
 * - `mode="build"`: the value is one build's `app_id` ("" = none picked); `builds` limits and orders
 *   the choice (apps in the order of their first build there), otherwise every build, apps by name.
 * - `mode="app"`: the value is an app key (`game_key`); with `allowAll`, "" stands for "All apps".
 */
export function AppCombobox({
  mode,
  value,
  onChange,
  builds,
  allowAll,
  placeholder,
  className,
  disabled,
  id,
}: {
  mode: "build" | "app"
  value: string
  onChange: (value: string) => void
  builds?: GameApp[]
  allowAll?: boolean
  placeholder?: string
  className?: string
  disabled?: boolean
  id?: string
}) {
  const [open, setOpen] = useState(false)
  const { groups: all } = useApps()

  const groups = useMemo((): AppGroup[] => {
    if (!builds) return all || []
    // Keep the caller's order and only its builds; builds the shared list does not know yet get
    // an app of their own.
    const groupOf = new Map<string, AppGroup>()
    for (const g of all || []) for (const a of g.apps) groupOf.set(a.app_id, g)
    const picked = new Map<string, AppGroup>()
    for (const a of builds) {
      const g = groupOf.get(a.app_id)
      const key = g?.key || a.game_key || a.app_id
      const out = picked.get(key) || { key, name: g?.name || key, apps: [] }
      out.apps.push(a)
      picked.set(key, out)
    }
    return [...picked.values()]
  }, [all, builds])

  // The picked app, and build in build mode (a linear scan: at most a few hundred builds).
  const group = !value ? undefined : mode === "app" ? groups.find((g) => g.key === value) : groups.find((g) => g.apps.some((a) => a.app_id === value))
  const selected = group ? { group, build: mode === "build" ? group.apps.find((a) => a.app_id === value) : undefined } : null

  // Highlighted when the list opens.
  const current = selected ? (selected.build ? `build:${selected.build.app_id}` : `app:${selected.group.key}`) : mode === "app" && allowAll && !value ? ALL_VALUE : undefined

  const pick = (v: string) => {
    onChange(v)
    setOpen(false)
  }

  const build = selected?.build
  // The app id only when another build of the app has the same store.
  const ambiguous = !!build && selected.group.apps.some((a) => a !== build && buildLabel(a) === buildLabel(build))
  const label = selected ? (
    build ? (
      <>
        <span className="min-w-0 truncate">{selected.group.name}</span>
        <span className="shrink-0 text-muted-foreground">· {buildLabel(build)}</span>
        {ambiguous && <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">{build.app_id}</span>}
      </>
    ) : (
      <span className="truncate">{selected.group.name}</span>
    )
  ) : mode === "app" && allowAll && !value ? (
    <span className="truncate">All apps</span>
  ) : value ? (
    <span className="truncate font-mono text-xs">{value}</span>
  ) : (
    <span className="truncate text-muted-foreground">{placeholder ?? (mode === "build" ? "Pick a build" : "Pick an app")}</span>
  )

  return (
    // Modal: the list scrolls with the wheel inside dialogs too (the dialog blocks scrolling elsewhere).
    <Popover open={open} onOpenChange={setOpen} modal>
      <PopoverTrigger
        id={id}
        disabled={disabled}
        role="combobox"
        aria-expanded={open}
        title={build?.app_id ?? (selected && selected.group.name !== selected.group.key ? selected.group.key : undefined)}
        className={cn(
          "flex h-8 w-full min-w-0 items-center justify-between gap-1.5 rounded-lg border border-input bg-transparent py-2 pr-2 pl-2.5 text-sm whitespace-nowrap transition-colors outline-none select-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-input/30 dark:hover:bg-input/50",
          className,
        )}
      >
        <span className="flex min-w-0 flex-1 items-baseline gap-1.5 text-left">{label}</span>
        <ChevronDownIcon className="pointer-events-none size-4 shrink-0 text-muted-foreground" />
      </PopoverTrigger>
      <PopoverContent align="start" className="w-(--radix-popover-trigger-width) min-w-72 p-0">
        <Command filter={filter} loop defaultValue={current}>
          <CommandInput placeholder="App name, key or bundle id…" />
          <CommandList className="max-h-80">
            <CommandEmpty>No app found.</CommandEmpty>
            {mode === "app" && allowAll && (
              <CommandGroup>
                <CommandItem value={ALL_VALUE} keywords={["all apps"]} data-checked={!value} onSelect={() => pick("")}>
                  <LayersIcon className="text-muted-foreground" />
                  All apps
                </CommandItem>
              </CommandGroup>
            )}
            {mode === "app" ? (
              <CommandGroup heading="Apps">
                {groups.map((g) => (
                  <CommandItem key={g.key} value={`app:${g.key}`} keywords={[appSearchText(g)]} data-checked={g.key === value} onSelect={() => pick(g.key)}>
                    <span className="flex min-w-0 flex-1 flex-col">
                      <span className="truncate">{g.name}</span>
                      <span className="truncate font-mono text-xs text-muted-foreground">
                        {g.name !== g.key ? `${g.key} · ` : ""}{g.apps.map((a) => a.app_id).join(" · ")}
                      </span>
                    </span>
                  </CommandItem>
                ))}
              </CommandGroup>
            ) : (
              groups.map((g) => {
                // The app's name and key, and this build's id only: "garden ios" finds the iOS build.
                const text = `${g.name} ${g.key}`
                return (
                  <CommandGroup key={g.key} heading={g.name !== g.key ? `${g.name} · ${g.key}` : g.name}>
                    {g.apps.map((a) => {
                      const Icon = PLATFORM_ICON[a.platform]
                      return (
                        <CommandItem
                          key={a.app_id}
                          value={`build:${a.app_id}`}
                          keywords={[`${text} ${a.app_id} ${buildLabel(a)} ${a.platform}`]}
                          data-checked={a.app_id === value}
                          onSelect={() => pick(a.app_id)}
                        >
                          {Icon ? <Icon className="text-muted-foreground" /> : null}
                          <span className="shrink-0">{buildLabel(a)}</span>
                          <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">{a.app_id}</span>
                        </CommandItem>
                      )
                    })}
                  </CommandGroup>
                )
              })
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  )
}
