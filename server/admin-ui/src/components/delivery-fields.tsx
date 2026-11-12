import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Field } from "@/components/common"
import type { DeliverySettings } from "@/lib/delivery"
import type { Delivery } from "@/lib/types"

type TextKey = "spacetime_url" | "spacetime_db" | "grant_reducer" | "revoke_reducer" | "webhook_url"

/** Delivery target of an app (Delivery tab, setup wizard); the keys it needs are set elsewhere. */
export function DeliveryFields({ value, onChange }: { value: DeliverySettings; onChange: (patch: Partial<DeliverySettings>) => void }) {
  const text = (k: TextKey, label: string, className?: string) => (
    <Field label={label} className={className}>
      <Input value={value[k]} onChange={(e) => onChange({ [k]: e.target.value })} />
    </Field>
  )
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <Field label="Delivery">
        <Select value={value.delivery} onValueChange={(v) => onChange({ delivery: v as Delivery })}>
          <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="spacetimedb">Reducers in the app's SpacetimeDB database</SelectItem>
            <SelectItem value="webhook">Webhook to the app backend</SelectItem>
            <SelectItem value="none">No backend: the client grants rewards</SelectItem>
          </SelectContent>
        </Select>
      </Field>
      {value.delivery === "spacetimedb" && (
        <>
          {text("spacetime_url", "SpacetimeDB server (https, allowed host)")}
          {text("spacetime_db", "Database name")}
          {text("grant_reducer", "Grant reducer")}
          {text("revoke_reducer", "Revoke reducer")}
          <Field label="Arguments format" hint="legacy: only for apps migrated from Adapty">
            <Select value={value.args_format} onValueChange={(v) => onChange({ args_format: v })}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent><SelectItem value="v1">v1</SelectItem><SelectItem value="legacy">legacy</SelectItem></SelectContent>
            </Select>
          </Field>
        </>
      )}
      {value.delivery === "webhook" && text("webhook_url", "Webhook URL (https)", "sm:col-span-2")}
    </div>
  )
}
