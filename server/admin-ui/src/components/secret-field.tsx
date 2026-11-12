import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import { cn } from "@/lib/utils"

/**
 * Write-only input of a secret: what is typed or pasted is masked and nothing stored is ever shown.
 * `file`: a key file (JSON key, .p8) that keeps its line breaks, so a masked textarea plus a file
 * picker instead of a password field.
 */
export function SecretField({ value, onChange, file, accept, placeholder, autoFocus, className }: {
  value: string
  onChange: (value: string) => void
  file?: boolean
  accept?: string
  placeholder?: string
  autoFocus?: boolean
  className?: string
}) {
  if (!file) {
    return (
      <Input type="password" autoComplete="new-password" spellCheck={false} value={value} placeholder={placeholder} autoFocus={autoFocus}
        className={cn("font-mono text-xs", className)} onChange={(e) => onChange(e.target.value)} />
    )
  }
  return (
    <div className={cn("grid gap-2", className)}>
      <Textarea value={value} autoComplete="off" spellCheck={false} placeholder={placeholder ?? "Paste the key or pick the file"} autoFocus={autoFocus}
        className="min-h-20 font-mono text-xs [-webkit-text-security:disc]" onChange={(e) => onChange(e.target.value)} />
      <Input type="file" accept={accept} className="text-xs" onChange={async (e) => { const f = e.target.files?.[0]; if (f) onChange(await f.text()) }} />
    </div>
  )
}
