import { createContext, createElement, useContext, useEffect, useMemo, useState, type ReactNode } from "react"
import { select } from "./api"
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

/** Reloads on window focus at most this often (another admin may have added an app). */
const FOCUS_RELOAD_MS = 60_000

/** Loads `game_app` once for every page, again whenever `appsChanged()` is called, and on window
 * focus at most once a minute. A reload that finds nothing new keeps the previous state, so pages
 * keyed on the app list do not reload. */
export function AppsProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AppsState>({ apps: null, groups: null, names: {}, error: "" })
  useEffect(() => {
    let alive = true
    let seq = 0
    let loadedAt = 0
    let last = ""
    const load = () => {
      const mine = ++seq
      loadedAt = Date.now()
      // A module older than app_profile has no such table: no names then.
      const names = select<{ game_key: string; display_name: string }>({ table: "app_profile" })
        .then((rows) => Object.fromEntries(rows.map((r) => [r.game_key, r.display_name])) as AppNames)
        .catch(() => ({}) as AppNames)
      Promise.all([select<GameApp>({ table: "game_app" }), names])
        .then(([apps, names]) => {
          if (!alive || mine !== seq) return
          const snapshot = JSON.stringify([apps, names])
          if (snapshot === last) return setState((s) => (s.error ? { ...s, error: "" } : s))
          last = snapshot
          setState({ apps, groups: groupApps(apps, names), names, error: "" })
        })
        .catch((e) => alive && mine === seq && setState((s) => ({ ...s, error: errorText(e) })))
    }
    const onFocus = () => {
      if (Date.now() - loadedAt >= FOCUS_RELOAD_MS) load()
    }
    load()
    window.addEventListener(CHANGED, load)
    window.addEventListener("focus", onFocus)
    return () => {
      alive = false
      window.removeEventListener(CHANGED, load)
      window.removeEventListener("focus", onFocus)
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

/** Accounts named by the apps, without "default". */
export function accountsOf(apps: { account: string }[]): string[] {
  return [...new Set(apps.map((a) => a.account).filter((a) => a && a !== "default"))].sort()
}

/** Lower-case text an app is found by: display name, key and every build's app id. */
export function appSearchText(g: AppGroup): string {
  return `${g.name} ${g.key} ${g.apps.map((a) => a.app_id).join(" ")}`.toLowerCase()
}

/**
 * How well `text` matches `query`: every word of the query appears in it, in any order
 * ("rings17 keys" finds "puzzle-17 → Keys"). 0: no match; 1: the text starts with the first word;
 * 0.5: any other match. An empty query matches everything (1).
 */
export function matchWords(text: string, query: string): number {
  const hay = text.toLowerCase()
  const words = query.toLowerCase().split(/\s+/).filter(Boolean)
  if (!words.length) return 1
  if (!words.every((w) => hay.includes(w))) return 0
  return hay.startsWith(words[0]) ? 1 : 0.5
}

/** Builds by app id, and the app (group) of each build, indexed once per app list. */
export function useAppIndex() {
  const { apps, groups } = useApps()
  return useMemo(() => {
    const builds = new Map<string, GameApp>()
    const groupOf = new Map<string, AppGroup>()
    for (const a of apps || []) builds.set(a.app_id, a)
    for (const g of groups || []) for (const a of g.apps) groupOf.set(a.app_id, g)
    const byKey = new Map((groups || []).map((g) => [g.key, g]))
    return { builds, groupOf, byKey }
  }, [apps, groups])
}
