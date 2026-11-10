import { useState } from "react"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectSeparator, SelectTrigger, SelectValue } from "@/components/ui/select"

const NEW = "__new__"
const VALID = /^[A-Za-z0-9._-]{1,64}$/

/**
 * Developer account of a build: one of the existing accounts, or a new one typed by the admin.
 * Picking from a list avoids typos that would silently create an account without keys.
 */
export function AccountSelect({ value, accounts, onChange, className }: {
  value: string
  accounts: string[]
  onChange: (account: string) => void
  className?: string
}) {
  const known = [...new Set([...accounts, "default"])]
  const [typing, setTyping] = useState(!!value && !known.includes(value))
  const [draft, setDraft] = useState(typing ? value : "")
  if (typing) {
    return (
      <div className={className}>
        <Input
          autoFocus
          placeholder="new account, e.g. studio-name"
          value={draft}
          aria-invalid={!!draft && !VALID.test(draft)}
          onChange={(e) => {
            setDraft(e.target.value)
            if (VALID.test(e.target.value)) onChange(e.target.value)
          }}
          onBlur={() => { if (!draft) { setTyping(false); onChange(known[0]) } }}
        />
        <p className="mt-1 text-xs text-muted-foreground">Letters, digits, <code>.</code> <code>_</code> <code>-</code>. Its keys are set in Settings → Developer accounts.</p>
      </div>
    )
  }
  return (
    <Select value={known.includes(value) ? value : known[0]} onValueChange={(v) => { if (v === NEW) { setTyping(true); setDraft("") } else onChange(v) }}>
      <SelectTrigger className={className ?? "w-full"}><SelectValue /></SelectTrigger>
      <SelectContent>
        {known.map((a) => <SelectItem key={a} value={a}>{a === "default" ? "default (shared fallback)" : a}</SelectItem>)}
        <SelectSeparator />
        <SelectItem value={NEW}>New account…</SelectItem>
      </SelectContent>
    </Select>
  )
}

/** Accounts named by the apps, without "default". */
export function accountsOf(apps: { account: string }[]): string[] {
  return [...new Set(apps.map((a) => a.account).filter((a) => a && a !== "default"))].sort()
}
