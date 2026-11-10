import { useCallback, useEffect, useState } from "react"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { Shell } from "@/components/shell"
import { call, clearToken, getToken, SERVED_BY_MODULE, sql, whoami } from "@/lib/api"
import { AppsProvider } from "@/lib/apps"
import { errorText } from "@/lib/format"
import { ClaimPage, LoginPage, WrongPlacePage } from "@/pages/auth"
import { AppPage } from "@/pages/app"
import { PlayerPage } from "@/pages/player"
import { PAGES, parseRoute, type Route } from "@/routes"

type Auth = { state: "checking" } | { state: "login"; error?: string } | { state: "claim"; me: string } | { state: "ready"; me: string }


export default function App() {
  const [auth, setAuth] = useState<Auth>({ state: "checking" })
  const [route, setRoute] = useState<Route>(() => parseRoute(location.hash))

  const start = useCallback(async () => {
    if (!getToken()) return setAuth({ state: "login" })
    let admins: number
    try {
      admins = Number((await sql<{ n: number }>("SELECT COUNT(*) AS n FROM admin"))[0].n)
    } catch (e) {
      clearToken()
      return setAuth({ state: "login", error: "Token rejected: " + errorText(e) })
    }
    const me = await whoami()
    // The module was published over a database created in the dashboard: `init` never ran.
    if (admins === 0) return setAuth({ state: "claim", me })
    try {
      if (Number((await sql<{ n: number }>("SELECT COUNT(*) AS n FROM retry_schedule"))[0].n) === 0) await call("admin_ensure_worker")
    } catch {
      // Not an admin: pages show the error.
    }
    setAuth({ state: "ready", me })
  }, [])

  useEffect(() => {
    if (SERVED_BY_MODULE) void start()
  }, [start])

  useEffect(() => {
    const onHash = () => setRoute(parseRoute(location.hash))
    window.addEventListener("hashchange", onHash)
    return () => window.removeEventListener("hashchange", onHash)
  }, [])

  const go = useCallback((r: string) => {
    if (location.hash.slice(1) === r) setRoute(parseRoute(r))
    else location.hash = r
  }, [])

  const signOut = useCallback(() => {
    clearToken()
    setAuth({ state: "login" })
  }, [])

  let content
  if (!SERVED_BY_MODULE) content = <WrongPlacePage />
  else if (auth.state === "checking") content = <div className="flex min-h-svh items-center justify-center text-sm text-muted-foreground">Connecting…</div>
  else if (auth.state === "login") content = <LoginPage error={auth.error} onSignedIn={start} />
  else if (auth.state === "claim") content = <ClaimPage me={auth.me} onDone={start} onSignOut={signOut} />
  else {
    const Page = PAGES[route.page]?.component
    content = (
      <AppsProvider>
        <Shell route={route} go={go} me={auth.me} onSignOut={signOut}>
          {"key" in route ? (
            <AppPage key={route.key} appKey={route.key} tab={route.tab} go={go} me={auth.me} />
          ) : "userId" in route ? (
            <PlayerPage key={route.userId} userId={route.userId} go={go} me={auth.me} />
          ) : (
            Page && <Page key={route.page} go={go} me={auth.me} />
          )}
        </Shell>
      </AppsProvider>
    )
  }

  return (
    <TooltipProvider>
      {content}
      <Toaster richColors position="bottom-right" />
    </TooltipProvider>
  )
}

