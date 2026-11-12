import { STORE_SECRETS } from "./stores"

/** Secrets whose value is a file (JSON key, .p8) rather than a short string. */
const FILE_SECRETS = new Set([...STORE_SECRETS.filter((s) => s.file).map((s) => s.name), "gcs_service_account_json"])

export const isFileSecret = (name: string) => FILE_SECRETS.has(name)

/**
 * Where a key a build needs is saved without asking (the setup wizard): the build's developer
 * account when the account has no such key yet, otherwise the build itself. A key that other apps
 * already read (an account's, or the shared `default`) is never replaced; the Keys tab is where an
 * admin picks a shared scope on purpose.
 */
export function ownSecretScope(name: string, build: { app_id: string; account: string }, creds: ReadonlySet<string>): string {
  const account = build.account || "default"
  return account !== "default" && !creds.has(`${account}/${name}`) ? account : build.app_id
}
