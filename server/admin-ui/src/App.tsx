import { useCallback, useEffect, useState } from "react"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { Shell } from "@/components/shell"
import { call, clearToken, getToken, isAuthError, proc, select, SERVED_BY_MODULE, setReadThroughProcedure, setToken, SIGNED_OUT_EVENT, tokenFromProvider, whoami } from "@/lib/api"
import { claimsOf, finishSignIn } from "@/lib/sso"
import { can, RoleContext, type Role, ScopeContext } from "@/lib/roles"
import { AppsProvider } from "@/lib/apps"
import { errorText } from "@/lib/format"
import { ClaimPage, LoginPage, UnreachablePage, WrongPlacePage } from "@/pages/auth"
import { AppPage } from "@/pages/app"
import { PlayerPage } from "@/pages/player"
import { PAGES, parseRoute, type Route } from "@/routes"

type Auth =
  | { state: "checking" }
  | { state: "login"; error?: string }
  | { state: "unreachable"; error: string }
  | { state: "claim"; me: string }
  | { state: "ready"; me: string; role: Role; scope: string[] }

interface Me {
  identity: string
  name: string
  role: Role
  owner: boolean
  /** The apps the admin is limited to; empty = every app. */
  game_keys: string[]
}

/** Signs in with the provider's token, claiming an invitation of its email when the caller is not
 * an admin yet. */
async function meThroughProvider(): Promise<Me> {
  try {
    return await proc<Me>("admin_me")
  } catch (e) {
    if (!isAuthError(e)) throw e
    await call("admin_accept_invite")
    return await proc<Me>("admin_me")
  }
}

/** Checks the stored token with the server. Only a rejected token is forgotten: a network or server
 * failure keeps it, so a page reload during an outage does not sign the admin out. */
async function signIn(): Promise<Auth> {
  try {
    const idToken = await finishSignIn()
    if (idToken) setToken(idToken, true)
  } catch (e) {
    return { state: "login", error: "Sign-in failed: " + errorText(e) }
  }
  if (!getToken()) return { state: "login" }
  setReadThroughProcedure(false)
  let me: Me
  if (tokenFromProvider()) {
    // Provider tokens are short-lived: an expired one is refused by the server anyway.
    if (Number(claimsOf(getToken()).exp || 0) * 1000 < Date.now()) {
      clearToken()
      return { state: "login", error: "Your sign-in has expired: sign in again." }
    }
    try {
      me = await meThroughProvider()
    } catch (e) {
      if (!isAuthError(e) && !/ERR_NOT_INVITED|ERR_SIGN_IN|ERR_ADMIN/.test(errorText(e))) return { state: "unreachable", error: errorText(e) }
      const email = String(claimsOf(getToken()).email || "")
      clearToken()
      return { state: "login", error: (email ? `${email}: ` : "") + errorText(e) }
    }
  } else try {
    me = await proc<Me>("admin_me")
  } catch (e) {
    // The owner of a database the module was published over (created in the dashboard): `init` never ran, there is no admin yet.
    try {
      if ((await select({ table: "admin" })).length === 0) return { state: "claim", me: await whoami() }
    } catch {
      // Not the owner either.
    }
    if (!isAuthError(e)) return { state: "unreachable", error: errorText(e) }
    clearToken()
    return { state: "login", error: "Token rejected: " + errorText(e) }
  }
  // Only the owner can read private tables over /sql; everyone else reads through `admin_select`.
  setReadThroughProcedure(!me.owner)
  try {
    if (can(me.role, "configure") && (await select({ table: "retry_schedule" })).length === 0) await call("admin_ensure_worker")
  } catch {
    // Pages show errors.
  }
  return { state: "ready", me: me.identity.replace(/^0x/, "").toLowerCase(), role: me.role, scope: me.game_keys || [] }
}

export default function App() {
  const [auth, setAuth] = useState<Auth>({ state: "checking" })
  const [route, setRoute] = useState<Route>(() => parseRoute(location.hash))

  const start = useCallback(async () => setAuth(await signIn()), [])
  const retry = useCallback(() => {
    setAuth({ state: "checking" })
    void start()
  }, [start])

  useEffect(() => {
    if (!SERVED_BY_MODULE) return
    let alive = true
    void signIn().then((a) => alive && setAuth(a))
    return () => {
      alive = false
    }
  }, [])

  useEffect(() => {
    const onSignedOut = () => {
      clearToken()
      setAuth({ state: "login", error: "Your sign-in has expired: sign in again." })
    }
    window.addEventListener(SIGNED_OUT_EVENT, onSignedOut)
    return () => window.removeEventListener(SIGNED_OUT_EVENT, onSignedOut)
  }, [])

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
  else if (auth.state === "unreachable") content = <UnreachablePage error={auth.error} onRetry={retry} onSignOut={signOut} />
  else if (auth.state === "claim") content = <ClaimPage me={auth.me} onDone={start} onSignOut={signOut} />
  else {
    const def = PAGES[route.page]
    const Page = def && (!def.need || can(auth.role, def.need)) && !(def.everyApp && auth.scope.length) ? def.component : NotForRole
    content = (
      <RoleContext.Provider value={auth.role}>
      <ScopeContext.Provider value={auth.scope}>
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
      </ScopeContext.Provider>
      </RoleContext.Provider>
    )
  }

  return (
    <TooltipProvider>
      {content}
      <Toaster richColors position="bottom-right" />
    </TooltipProvider>
  )
}

function NotForRole() {
  return <div className="p-6 text-sm text-muted-foreground">Your role cannot open this page.</div>
}
