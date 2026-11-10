import type { ComponentType } from "react"
import {
  BadgeDollarSignIcon, BellRingIcon, BuildingIcon, LayoutDashboardIcon, LayoutGridIcon, ReceiptTextIcon, ScrollTextIcon, ServerIcon, ShieldCheckIcon,
} from "lucide-react"
import { OverviewPage } from "@/pages/overview"
import { RevenuePage } from "@/pages/revenue"
import { PurchasesPage } from "@/pages/purchases"
import { AppsPage } from "@/pages/apps"
import { AccountsPage } from "@/pages/accounts"
import { ServerPage } from "@/pages/server"
import { LogsPage } from "@/pages/logs"
import { AdminsPage } from "@/pages/admins"
import { AlertsPage } from "@/pages/alerts"

export interface PageProps {
  go: (route: string) => void
  me: string
}

export interface PageDef {
  title: string
  icon: ComponentType<{ className?: string }>
  component: ComponentType<PageProps>
  group: "Monitor" | "Apps" | "Settings"
}

/** Top-level pages. Apps (`#apps`, `#app/<key>/<tab>`) are listed separately in the sidebar. */
export const PAGES: Record<string, PageDef> = {
  overview: { title: "Overview", icon: LayoutDashboardIcon, component: OverviewPage, group: "Monitor" },
  revenue: { title: "Revenue", icon: BadgeDollarSignIcon, component: RevenuePage, group: "Monitor" },
  purchases: { title: "Purchases", icon: ReceiptTextIcon, component: PurchasesPage, group: "Monitor" },
  apps: { title: "All apps", icon: LayoutGridIcon, component: AppsPage, group: "Apps" },
  accounts: { title: "Developer accounts", icon: BuildingIcon, component: AccountsPage, group: "Settings" },
  server: { title: "Server", icon: ServerIcon, component: ServerPage, group: "Settings" },
  logs: { title: "Logs", icon: ScrollTextIcon, component: LogsPage, group: "Settings" },
  admins: { title: "Admins", icon: ShieldCheckIcon, component: AdminsPage, group: "Settings" },
  alerts: { title: "Alerts", icon: BellRingIcon, component: AlertsPage, group: "Settings" },
}

export const APP_TABS = ["setup", "products", "paywalls", "delivery", "integrations", "keys", "purchases", "revenue"] as const
export type AppTab = (typeof APP_TABS)[number]

export type Route =
  | { page: string }
  | { page: "app"; key: string; tab: AppTab }
  | { page: "player"; userId: string }

export function parseRoute(hash: string): Route {
  const h = hash.replace(/^#/, "")
  if (h.startsWith("app/")) {
    const [, key, tab] = h.split("/")
    const t = (APP_TABS as readonly string[]).includes(tab) ? (tab as AppTab) : "setup"
    if (key) return { page: "app", key: decodeURIComponent(key), tab: t }
  }
  // `#player/<user_id>` (see `playerRoute` in lib/player.ts): the id is URL-encoded and may contain `/`.
  if (h.startsWith("player/") && h.length > 7) {
    try {
      return { page: "player", userId: decodeURIComponent(h.slice(7)) }
    } catch {
      // Malformed escape: fall through to the overview.
    }
  }
  const path = h.split("?")[0]
  return { page: PAGES[path] ? path : "overview" }
}

export const appRoute = (key: string, tab: AppTab = "setup") => `app/${encodeURIComponent(key)}/${tab}`
export const playerRoute = (userId: string) => `player/${encodeURIComponent(userId)}`
