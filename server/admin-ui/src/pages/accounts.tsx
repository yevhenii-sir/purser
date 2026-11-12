import { useState } from "react"
import { TriangleAlertIcon } from "lucide-react"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { DataTable, ErrorBox, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { SecretDialog, type SecretTarget } from "@/components/secret-dialog"
import { select } from "@/lib/api"
import { groupApps, useApps } from "@/lib/apps"
import { useAsync } from "@/lib/hooks"
import { storeOf, storeOfSecret, STORE_SECRETS } from "@/lib/stores"
import { appRoute, type PageProps } from "@/routes"

/** Keys a developer account usually shares between its apps: every store's account-wide keys
 * (from the store registry), then delivery and integration keys. */
const SHARED: { name: string; when: string; store?: string }[] = [
  ...STORE_SECRETS.filter((s) => s.account_wide).map((s) => {
    const store = storeOfSecret(s.name)!
    return { name: s.name, when: `${store.name} apps`, store: store.id }
  }),
  { name: "spacetime_token", when: "delivery to SpacetimeDB" },
  { name: "webhook_secret", when: "delivery to a webhook" },
  { name: "appsflyer_s2s_key", when: "AppsFlyer" },
  { name: "tenjin_sdk_key", when: "Tenjin" },
]

/** Keys that belong to one store developer account (Google Play, App Store Connect, Amazon, ...). */
const STORE_KEYS = STORE_SECRETS.filter((s) => s.account_wide).map((s) => s.name)

export function AccountsPage({ go }: PageProps) {
  const [target, setTarget] = useState<SecretTarget | null>(null)
  const { apps, error: appsError } = useApps()
  const creds = useAsync(async () => new Set((await select<{ key: string }>({ table: "credential", columns: ["key"] })).map((c) => c.key)))
  const accounts = [...new Set([...(apps || []).map((a) => a.account || "default"), "default"])].sort((a, b) => (a === "default" ? 1 : b === "default" ? -1 : a.localeCompare(b)))
  const data = apps && creds.data ? { apps, accounts, creds: creds.data } : undefined
  const error = appsError || creds.error
  const loading = !data
  const reload = creds.reload

  return (
    <>
      <PageHeader
        title="Developer accounts"
        description={<>Keys shared by every app of a developer account (Google Play / App Store Connect / Amazon Appstore / AppsFlyer / Tenjin), so the next app reuses them. An app's own key overrides the account's; <code>default</code> applies to apps whose account has none.</>}
      />
      {error ? <ErrorBox error={error} /> : loading || !data ? <Loading rows={4} /> : data.accounts.map((acc) => {
        const apps = data.apps.filter((a) => (a.account || "default") === acc)
        const groups = groupApps(apps)
        const stores = new Set(apps.map((a) => storeOf(a).id))
        const relevant = SHARED.filter((s) =>
          acc === "default" || data.creds.has(`${acc}/${s.name}`) || !s.store || stores.has(s.store))
        const named = data.accounts.filter((a) => a !== "default")
        const storeKeysInDefault = acc === "default" && named.length > 0
          ? STORE_KEYS.filter((k) => data.creds.has(`default/${k}`))
          : []
        return (
          <Section
            key={acc}
            title={acc === "default" ? "default" : <>Account <Mono className="text-sm">{acc}</Mono></>}
            description={
              groups.length ? (
                <span className="flex flex-wrap items-center gap-1.5">
                  Used by
                  {groups.map((g) => <Button key={g.key} variant="link" className="h-auto p-0 text-sm" onClick={() => go(appRoute(g.key, "keys"))}>{g.key}</Button>)}
                </span>
              ) : acc === "default" ? "Fallback for every app." : "No apps use this account."
            }
          >
            {storeKeysInDefault.length > 0 && (
              <Alert className="mb-4">
                <TriangleAlertIcon />
                <AlertTitle>Store keys in default</AlertTitle>
                <AlertDescription>
                  {storeKeysInDefault.join(", ")} {storeKeysInDefault.length > 1 ? "are" : "is"} used by every build whose account has no key of its own,
                  so apps of {named.join(", ")} may call a store with the wrong account's key. Set each account's store keys on its own card and remove them from default (Settings → Server → Advanced).
                </AlertDescription>
              </Alert>
            )}
            <DataTable
              rows={relevant}
              columns={[
                { title: "Key", cell: (s) => <Mono>{s.name}</Mono> },
                { title: "For", cell: (s) => <span className="text-muted-foreground">{s.when}</span> },
                { title: "", cell: (s) => (data.creds.has(`${acc}/${s.name}`) ? <Tone tone="ok">set</Tone> : <Tone tone="muted">not set</Tone>) },
                {
                  title: "",
                  className: "text-right",
                  cell: (s) => (
                    <Button size="xs" variant="outline" onClick={() => setTarget({ name: s.name, scopes: [{ value: acc, label: acc }] })}>
                      {data.creds.has(`${acc}/${s.name}`) ? "Replace" : "Set"}
                    </Button>
                  ),
                },
              ]}
            />
            <p className="mt-3 text-xs text-muted-foreground">Keys of a single build (RTDN secret, GA4 secret, webhook integration key) are on the app's Keys tab.</p>
          </Section>
        )
      })}
      <SecretDialog target={target} onClose={() => setTarget(null)} onSaved={reload} />
    </>
  )
}
