import { useState } from "react"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Field } from "@/components/common"
import { SecretField } from "@/components/secret-field"
import { call } from "@/lib/api"
import { tryAct } from "@/lib/hooks"
import { isFileSecret } from "@/lib/secrets"
import { SECRET_NAMES } from "@/lib/types"

export interface SecretTarget {
  name: string
  /** Scopes to offer, first = default: store app id, account name or "default". */
  scopes: { value: string; label: string }[]
}

export function SecretDialog({ target, onClose, onSaved }: { target: SecretTarget | null; onClose: () => void; onSaved: () => void }) {
  return (
    <Dialog open={!!target} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-lg">{target && <SecretForm key={target.name} target={target} onClose={onClose} onSaved={onSaved} />}</DialogContent>
    </Dialog>
  )
}

function SecretForm({ target, onClose, onSaved }: { target: SecretTarget; onClose: () => void; onSaved: () => void }) {
  const [scope, setScope] = useState(target.scopes[0]?.value || "default")
  const [value, setValue] = useState("")
  const save = async () => {
    if (!value.trim()) return
    const ok = await tryAct(() => call("admin_set_credential", scope, target.name, value.trim()), "Saved")
    if (ok !== undefined) { onSaved(); onClose() }
  }
  return (
    <>
      <DialogHeader>
        <DialogTitle className="font-mono text-base">{target.name}</DialogTitle>
        <DialogDescription>{SECRET_NAMES.find(([n]) => n === target.name)?.[1]}. Stored on the server and never shown again.</DialogDescription>
      </DialogHeader>
      <div className="grid gap-4">
        {target.scopes.length > 1 && (
          <Field label="Used by">
            <Select value={scope} onValueChange={setScope}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>{target.scopes.map((s) => <SelectItem key={s.value} value={s.value}>{s.label}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
        )}
        <Field label="Value">
          <SecretField value={value} onChange={setValue} file={isFileSecret(target.name)} autoFocus />
        </Field>
      </div>
      <DialogFooter><Button onClick={save} disabled={!value.trim()}>Save</Button></DialogFooter>
    </>
  )
}
