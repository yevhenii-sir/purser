import { useEffect, useMemo, useState, type ReactNode } from "react"
import { useTheme } from "next-themes"
import { LogOutIcon, MonitorIcon, MoonIcon, PlusIcon, SearchIcon, SunIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { CommandMenu } from "@/components/command-menu"
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Separator } from "@/components/ui/separator"
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupAction, SidebarGroupContent, SidebarGroupLabel, SidebarHeader, SidebarInput,
  SidebarInset, SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarMenuSub, SidebarMenuSubButton, SidebarMenuSubItem,
  SidebarProvider, SidebarTrigger,
} from "@/components/ui/sidebar"
import { DB_NAME, DEMO, moduleVersion } from "@/lib/api"
import { useGroups } from "@/lib/apps"
import { appRoute, PAGES, type PageDef, type Route } from "@/routes"

function ThemeMenu() {
  const { theme, setTheme } = useTheme()
  const Icon = theme === "light" ? SunIcon : theme === "dark" ? MoonIcon : MonitorIcon
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="Theme">
          <Icon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem onClick={() => setTheme("light")}><SunIcon /> Light</DropdownMenuItem>
        <DropdownMenuItem onClick={() => setTheme("dark")}><MoonIcon /> Dark</DropdownMenuItem>
        <DropdownMenuItem onClick={() => setTheme("system")}><MonitorIcon /> System</DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function PageLinks({ group, route, go }: { group: PageDef["group"]; route: Route; go: (r: string) => void }) {
  return (
    <>
      {Object.entries(PAGES)
        .filter(([, p]) => p.group === group)
        .map(([key, p]) => (
          <SidebarMenuItem key={key}>
            <SidebarMenuButton isActive={route.page === key} tooltip={p.title} onClick={() => go(key)}>
              <p.icon />
              <span>{p.title}</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        ))}
    </>
  )
}

/** Apps listed in the sidebar before "N more…"; above it the sidebar shows a filter. */
const SIDEBAR_APPS = 12

export function Shell({ route, go, me, onSignOut, children }: {
  route: Route
  go: (route: string) => void
  me: string
  onSignOut: () => void
  children: ReactNode
}) {
  const { groups } = useGroups()
  const [filter, setFilter] = useState("")
  const [palette, setPalette] = useState(false)
  const [version, setVersion] = useState("")
  useEffect(() => {
    moduleVersion().then(setVersion)
  }, [])
  const title = "key" in route ? route.key : "userId" in route ? route.userId : PAGES[route.page]?.title
  const all = groups || []
  /** With many apps the sidebar shows a search box and only the first ones (plus the open app). */
  const visible = useMemo(() => {
    const f = filter.trim().toLowerCase()
    if (f) return all.filter((g) => g.key.toLowerCase().includes(f) || g.apps.some((a) => a.app_id.toLowerCase().includes(f)))
    const first = all.slice(0, SIDEBAR_APPS)
    const open = "key" in route ? all.find((g) => g.key === route.key) : undefined
    return open && !first.includes(open) ? [...first, open] : first
  }, [all, filter, route])
  const hidden = filter.trim() ? 0 : all.length - visible.length
  return (
    <SidebarProvider>
      <Sidebar collapsible="icon">
        <SidebarHeader className="group-data-[collapsible=icon]:hidden">
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton size="lg" onClick={() => go("overview")}>
                <div className="grid flex-1 text-left text-sm leading-tight">
                  <span className="flex min-w-0 items-baseline gap-1.5">
                    <span className="truncate font-semibold">Purser</span>
                    {version && (
                      <span className="shrink-0 font-mono text-[11px] text-muted-foreground" title="Billing module version">v{version}</span>
                    )}
                  </span>
                  <span className="truncate text-xs text-muted-foreground">{DB_NAME}</span>
                </div>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarHeader>
        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupLabel>Monitor</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu><PageLinks group="Monitor" route={route} go={go} /></SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
          <SidebarGroup>
            <SidebarGroupLabel>Apps</SidebarGroupLabel>
            <SidebarGroupAction title="Add an app" onClick={() => go("apps?new")}>
              <PlusIcon /> <span className="sr-only">Add an app</span>
            </SidebarGroupAction>
            <SidebarGroupContent>
              {all.length > SIDEBAR_APPS && (
                <div className="px-2 pb-1 group-data-[collapsible=icon]:hidden">
                  <SidebarInput placeholder="Filter apps…" value={filter} onChange={(e) => setFilter(e.target.value)} />
                </div>
              )}
              <SidebarMenu>
                <PageLinks group="Apps" route={route} go={go} />
                {(visible.length > 0 || hidden > 0) && (
                  <SidebarMenuItem>
                    {/* A tree under "All apps"; hidden when the sidebar is collapsed to icons. */}
                    <SidebarMenuSub>
                      {visible.map((g) => (
                        <SidebarMenuSubItem key={g.key}>
                          <SidebarMenuSubButton asChild isActive={"key" in route && route.key === g.key}>
                            <button type="button" title={g.apps.map((a) => a.app_id).join(", ")} onClick={() => go(appRoute(g.key, "key" in route ? route.tab : "setup"))}>
                              <span>{g.key}</span>
                            </button>
                          </SidebarMenuSubButton>
                        </SidebarMenuSubItem>
                      ))}
                      {hidden > 0 && (
                        <SidebarMenuSubItem>
                          <SidebarMenuSubButton asChild className="text-muted-foreground">
                            <button type="button" onClick={() => go("apps")}><span>{hidden} more…</span></button>
                          </SidebarMenuSubButton>
                        </SidebarMenuSubItem>
                      )}
                    </SidebarMenuSub>
                  </SidebarMenuItem>
                )}
                {filter.trim() && !visible.length && <div className="px-2 py-1 text-xs text-muted-foreground group-data-[collapsible=icon]:hidden">No app matches</div>}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
          <SidebarGroup>
            <SidebarGroupLabel>Settings</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu><PageLinks group="Settings" route={route} go={go} /></SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter>
          <SidebarMenu>
            <SidebarMenuItem>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <SidebarMenuButton tooltip="Account">
                    <div className="flex size-6 shrink-0 items-center justify-center rounded-full bg-muted font-mono text-[10px]">{me.slice(4, 6) || "?"}</div>
                    <span className="truncate font-mono text-xs">{me ? me.slice(0, 12) + "…" : "Account"}</span>
                  </SidebarMenuButton>
                </DropdownMenuTrigger>
                <DropdownMenuContent side="top" align="start" className="w-72">
                  <DropdownMenuLabel className="font-mono text-xs break-all">{me || "unknown identity"}</DropdownMenuLabel>
                  {!DEMO && <DropdownMenuSeparator />}
                  {!DEMO && <DropdownMenuItem onClick={onSignOut}><LogOutIcon /> Sign out</DropdownMenuItem>}
                </DropdownMenuContent>
              </DropdownMenu>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarFooter>
      </Sidebar>
      <SidebarInset className="min-w-0">
        <header className="sticky top-0 z-10 flex h-14 shrink-0 items-center gap-2 border-b bg-background/80 px-4 backdrop-blur">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-1 data-[orientation=vertical]:h-4" />
          {"key" in route ? (
            <nav className="flex min-w-0 items-center gap-1.5 text-sm">
              <button className="text-muted-foreground hover:text-foreground" onClick={() => go("apps")}>Apps</button>
              <span className="text-muted-foreground">/</span>
              <span className="truncate font-medium">{title}</span>
            </nav>
          ) : "userId" in route ? (
            <nav className="flex min-w-0 items-center gap-1.5 text-sm">
              <button className="shrink-0 text-muted-foreground hover:text-foreground" onClick={() => go("purchases")}>Players</button>
              <span className="text-muted-foreground">/</span>
              <span className="truncate font-mono text-xs font-medium">{title}</span>
            </nav>
          ) : (
            <span className="text-sm font-medium">{title}</span>
          )}
          <span className="flex-1" />
          {DEMO && (
            <a
              href="https://github.com/yevhenii-sir/purser#quick-start"
              target="_blank"
              rel="noreferrer"
              className="hidden items-center gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/10 px-2 py-1 text-xs text-amber-700 hover:bg-amber-500/15 md:inline-flex dark:text-amber-300"
              title="Made-up apps and purchases; changes are not saved. Click to set up your own server."
            >
              <span className="size-1.5 rounded-full bg-amber-500" /> Demo · sample data, read-only
            </a>
          )}
          <Button variant="outline" size="sm" className="gap-2 text-muted-foreground" onClick={() => setPalette(true)}>
            <SearchIcon /> <span className="hidden sm:inline">Go to app…</span>
            <kbd className="hidden rounded border bg-muted px-1 font-mono text-[10px] sm:inline">Ctrl K</kbd>
          </Button>
          <ThemeMenu />
        </header>
        <div className="mx-auto w-full max-w-7xl space-y-6 p-4 md:p-6">{children}</div>
      </SidebarInset>
      <CommandMenu open={palette} setOpen={setPalette} groups={all} go={go} />
    </SidebarProvider>
  )
}
