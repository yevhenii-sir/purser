import { useState, type ReactNode } from "react"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { call, DB_NAME, getToken, setToken, sql } from "@/lib/api"
import { tryAct, useAsync } from "@/lib/hooks"
import { providerName, signInConfig, SPACETIMEAUTH_ISSUER, startSignIn } from "@/lib/sso"

function Centered({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-svh flex-col items-center justify-center gap-6 bg-muted/30 p-6">
      <div className="font-semibold">Purser</div>
      <div className="w-full max-w-md">{children}</div>
    </div>
  )
}

export function LoginPage({ error, onSignedIn }: { error?: string; onSignedIn: () => void }) {
  const [token, setTokenValue] = useState("")
  const provider = useAsync(signInConfig)
  const [withToken, setWithToken] = useState(false)
  const [leaving, setLeaving] = useState(false)
  const submit = () => {
    if (!token.trim()) return
    setToken(token.trim())
    onSignedIn()
  }
  const config = provider.data
  const tokenForm = !config || withToken
  return (
    <Centered>
      <Card>
        <CardHeader>
          <CardTitle>Sign in</CardTitle>
          <CardDescription>
            {config ? (
              <>Admins of database <code className="font-mono">{DB_NAME}</code> sign in with the account they were invited with.</>
            ) : (
              <>Use the token of the owner of database <code className="font-mono">{DB_NAME}</code>. Get it from the CLI:</>
            )}
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {config && (
            <div className="space-y-2">
              <Button className="w-full" disabled={leaving} onClick={async () => {
                setLeaving(true)
                await tryAct(() => startSignIn(config))
                setLeaving(false)
              }}>
                Sign in with {providerName(config)}
              </Button>
              {config.issuer === SPACETIMEAUTH_ISSUER && (
                <p className="text-center text-xs text-muted-foreground">Google, GitHub, Discord and the other accounts this server allows.</p>
              )}
            </div>
          )}
          {error && (
            <Alert variant="destructive">
              <AlertDescription className="break-all">{error}</AlertDescription>
            </Alert>
          )}
          {config && !withToken && (
            <Button variant="link" size="sm" className="w-full text-muted-foreground" onClick={() => setWithToken(true)}>
              Sign in with a token instead
            </Button>
          )}
          {tokenForm && (
            <>
              {config && <p className="border-t pt-4 text-sm text-muted-foreground">The owner's or an admin's SpacetimeDB token, e.g. from the CLI:</p>}
              <pre className="rounded-md bg-muted px-3 py-2 font-mono text-xs">spacetime login show --token</pre>
              <div className="grid gap-1.5">
                <Label htmlFor="token">Token</Label>
                <Input id="token" type="password" autoComplete="off" spellCheck={false} value={token}
                  onChange={(e) => setTokenValue(e.target.value)} onKeyDown={(e) => e.key === "Enter" && submit()} />
              </div>
            </>
          )}
        </CardContent>
        <CardFooter className="flex-col items-stretch gap-3">
          {tokenForm && <Button variant={config ? "outline" : "default"} onClick={submit}>Sign in with the token</Button>}
          <p className="text-xs text-muted-foreground">
            The token is kept for this tab only and sent only to this database; it is forgotten when the tab is closed. Every
            database on a SpacetimeDB host shares this page's address: on a shared host such as maincloud, sign out when you are
            done and do not open other databases' pages in this browser meanwhile.
          </p>
        </CardFooter>
      </Card>
    </Centered>
  )
}

/**
 * The module was published over a database created in the dashboard, so `init` never ran. Only the
 * database owner can write private tables over /sql, so the owner can add itself as the first admin.
 */
export function ClaimPage({ me, onDone, onSignOut }: { me: string; onDone: () => void; onSignOut: () => void }) {
  const [busy, setBusy] = useState(false)
  const claim = async () => {
    if (!/^[0-9a-f]{64}$/.test(me)) return
    setBusy(true)
    const ok = await tryAct(async () => {
      await sql(`INSERT INTO admin (identity, protected, role, name, added_by, added_at_micros, game_keys) VALUES (0x${me}, true, 'admin', '', '', 0, '')`)
      await call("admin_ensure_worker")
      return true
    }, "You are the owner admin, the worker is running")
    setBusy(false)
    if (ok) onDone()
  }
  return (
    <Centered>
      <Card>
        <CardHeader>
          <CardTitle>Finish the installation</CardTitle>
          <CardDescription>
            This database has no admins and the background worker is not running: the module was published over a database created in the
            SpacetimeDB dashboard, and an update does not run the module's <code>init</code>.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-2 text-sm">
          <p>You are the database owner, so you can register yourself as its owner admin and start the worker.</p>
          <p className="font-mono text-xs break-all text-muted-foreground">{me || "Could not determine your identity."}</p>
        </CardContent>
        <CardFooter className="justify-between gap-2">
          <Button variant="ghost" onClick={onSignOut}>Sign out</Button>
          <Button onClick={claim} disabled={busy || !me || !getToken()}>Become the owner admin</Button>
        </CardFooter>
      </Card>
    </Centered>
  )
}

/** The sign-in check failed for another reason than the token (network, server error): the token is kept. */
export function UnreachablePage({ error, onRetry, onSignOut }: { error: string; onRetry: () => void; onSignOut: () => void }) {
  return (
    <Centered>
      <Card>
        <CardHeader>
          <CardTitle>Cannot reach the server</CardTitle>
          <CardDescription>The billing database did not answer the sign-in check. Your token is kept.</CardDescription>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive"><AlertDescription className="font-mono text-xs break-all">{error}</AlertDescription></Alert>
        </CardContent>
        <CardFooter className="justify-between gap-2">
          <Button variant="ghost" onClick={onSignOut}>Sign out</Button>
          <Button onClick={onRetry}>Try again</Button>
        </CardFooter>
      </Card>
    </Centered>
  )
}

export function WrongPlacePage() {
  const [url, setUrl] = useState("https://maincloud.spacetimedb.com/v1/database/<database>/route/admin")
  return (
    <Centered>
      <Card>
        <CardHeader>
          <CardTitle>Open the admin through the server</CardTitle>
          <CardDescription>
            This page only works at the address where the billing module serves it (<code>/v1/database/&lt;database&gt;/route/admin</code>).
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Input value={url} onChange={(e) => setUrl(e.target.value)} className="font-mono text-xs" />
        </CardContent>
        <CardFooter className="justify-end">
          <Button onClick={() => (location.href = url.trim())}>Open</Button>
        </CardFooter>
      </Card>
    </Centered>
  )
}
