import { createContext, createElement, useContext, useEffect, useState, type ReactNode } from "react"
import { sql } from "./api"
import { errorText } from "./format"
import { PLATFORMS } from "./stores"
import type { GameApp } from "./types"

/**
 * An app as people think of it: one product with any number of builds (Google Play, App Store,
 * Amazon Appstore, Steam, web). On the server it is the `game_app` rows that share a `game_key`.
 */
export interface AppGroup {
  key: string
  /** Display name (`app_profile`), or the key when it has none. */
  name: string
  /** Every build, ordered by platform (registry order), then app id. */
  apps: GameApp[]
  /** First Android (Google Play) build, if any. */
  android?: GameApp
  /** First iOS build, if any. */
  ios?: GameApp
}

const platformOrder = (p: string) => {
  const i = PLATFORMS.findIndex((x) => x.id === p)
  return i < 0 ? PLATFORMS.length : i
}

/** Display names by app key (`app_profile`). */
export type AppNames = Record<string, string>

export function groupApps(apps: GameApp[], names: AppNames = {}): AppGroup[] {
  const map = new Map<string, AppGroup>()
  for (const a of [...apps].sort((x, y) => platformOrder(x.platform) - platformOrder(y.platform) || x.app_id.localeCompare(y.app_id))) {
    const key = a.game_key || a.app_id
    const g = map.get(key) || { key, name: names[key] || key, apps: [] }
    g.apps.push(a)
    if (a.platform === "android" && !g.android) g.android = a
    if (a.platform === "ios" && !g.ios) g.ios = a
    map.set(key, g)
  }
  return [...map.values()].sort((a, b) => a.name.localeCompare(b.name) || a.key.localeCompare(b.key))
}

const CHANGED = "purser:apps-changed"

/** Tells the sidebar and other views that apps were added, renamed or removed. */
export function appsChanged() {
  window.dispatchEvent(new Event(CHANGED))
}

export interface AppsState {
  /** Every `game_app` row; null until loaded. */
  apps: GameApp[] | null
  groups: AppGroup[] | null
  /** Display names by app key. */
  names: AppNames
  error: string
}

const AppsContext = createContext<AppsState>({ apps: null, groups: null, names: {}, error: "" })

/** Loads `game_app` once for every page and again whenever `appsChanged()` is called. */
export function AppsProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AppsState>({ apps: null, groups: null, names: {}, error: "" })
  useEffect(() => {
    let alive = true
    let seq = 0
    const load = () => {
      const mine = ++seq
      // A module older than app_profile has no such table: no names then.
      const names = sql<{ game_key: string; display_name: string }>("SELECT * FROM app_profile")
        .then((rows) => Object.fromEntries(rows.map((r) => [r.game_key, r.display_name])) as AppNames)
        .catch(() => ({}) as AppNames)
      Promise.all([sql<GameApp>("SELECT * FROM game_app"), names])
        .then(([apps, names]) => alive && mine === seq && setState({ apps, groups: groupApps(apps, names), names, error: "" }))
        .catch((e) => alive && mine === seq && setState((s) => ({ ...s, error: errorText(e) })))
    }
    load()
    window.addEventListener(CHANGED, load)
    return () => {
      alive = false
      window.removeEventListener(CHANGED, load)
    }
  }, [])
  return createElement(AppsContext.Provider, { value: state }, children)
}

/** The shared `game_app` rows and app groups (see `AppsProvider`). */
export function useApps(): AppsState {
  return useContext(AppsContext)
}

/** Display name of an app key (the key itself when it has none). */
export function useAppName(): (key: string) => string {
  const { names } = useApps()
  return (key) => names[key] || key
}

/** App groups, reloaded whenever `appsChanged()` is called. */
export function useGroups() {
  const { groups, error } = useApps()
  return { groups, error }
}

/** Platform names by id (from the store registry). Prefer `buildLabel` for a build. */
export const PLATFORM_LABEL: Record<string, string> = Object.fromEntries(PLATFORMS.map((p) => [p.id, p.name]))
