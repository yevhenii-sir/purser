import { useState, type ReactNode } from "react"
import { CheckIcon, CopyIcon } from "lucide-react"
import { toast } from "sonner"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Skeleton } from "@/components/ui/skeleton"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Textarea } from "@/components/ui/textarea"
import { cn } from "@/lib/utils"
import type { Level } from "@/lib/types"

export function PageHeader({ title, description, actions }: { title: string; description?: ReactNode; actions?: ReactNode }) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-3">
      <div className="min-w-0">
        <h1 className="text-xl font-semibold tracking-tight">{title}</h1>
        {description && <p className="mt-1 max-w-3xl text-sm text-muted-foreground">{description}</p>}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </div>
  )
}

export function Section({ title, description, actions, children, className }: {
  title?: ReactNode
  description?: ReactNode
  actions?: ReactNode
  children: ReactNode
  className?: string
}) {
  return (
    <Card className={className}>
      {(title || actions) && (
        <CardHeader className="flex flex-row flex-wrap items-start justify-between gap-2">
          <div className="space-y-1">
            {title && <CardTitle>{title}</CardTitle>}
            {description && <CardDescription>{description}</CardDescription>}
          </div>
          {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
        </CardHeader>
      )}
      <CardContent>{children}</CardContent>
    </Card>
  )
}

export interface Column<T> {
  title: ReactNode
  cell: (row: T, index: number) => ReactNode
  className?: string
  num?: boolean
}

export function DataTable<T>({ rows, columns, onRowClick, empty = "Nothing here", rowKey }: {
  rows: T[]
  columns: Column<T>[]
  onRowClick?: (row: T) => void
  empty?: ReactNode
  rowKey?: (row: T, index: number) => string
}) {
  if (!rows.length) return <div className="py-8 text-center text-sm text-muted-foreground">{empty}</div>
  return (
    <Table>
      <TableHeader>
        <TableRow>
          {columns.map((c, i) => (
            <TableHead key={i} className={cn(c.num && "text-right", c.className)}>
              {c.title}
            </TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((r, i) => (
          <TableRow
            key={rowKey ? rowKey(r, i) : i}
            className={cn(onRowClick && "cursor-pointer")}
            onClick={(e) => {
              if (!onRowClick || (e.target as HTMLElement).closest("button, a, input, select, [role=combobox], [role=checkbox]")) return
              onRowClick(r)
            }}
          >
            {columns.map((c, j) => (
              <TableCell key={j} className={cn(c.num && "text-right tabular-nums", c.className)}>
                {c.cell(r, i)}
              </TableCell>
            ))}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  )
}

const STATUS_TONE: Record<string, string> = {
  completed: "ok",
  granted: "ok",
  verified: "warn",
  transfer_pending: "warn",
  revoke_pending: "warn",
  expire_pending: "warn",
  rejected: "bad",
  revoked: "bad",
}

const TONE_CLASS: Record<string, string> = {
  ok: "border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400",
  warn: "border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-400",
  bad: "border-red-500/30 bg-red-500/10 text-red-700 dark:text-red-400",
  muted: "",
}

export function Tone({ tone, children }: { tone: "ok" | "warn" | "bad" | "muted"; children: ReactNode }) {
  return (
    <Badge variant="outline" className={TONE_CLASS[tone]}>
      {children}
    </Badge>
  )
}

export function StatusBadge({ status }: { status: string }) {
  return <Tone tone={(STATUS_TONE[status] as "ok" | "warn" | "bad") || "muted"}>{status}</Tone>
}

export function LevelBadge({ level }: { level: Level }) {
  const tone = level === "ok" ? "ok" : level === "warn" ? "warn" : level === "fail" ? "bad" : "muted"
  return <Tone tone={tone}>{level}</Tone>
}

export function Mono({ children, className }: { children: ReactNode; className?: string }) {
  return <span className={cn("font-mono text-xs", className)}>{children}</span>
}

export function Field({ label, children, hint, className }: { label: ReactNode; children: ReactNode; hint?: ReactNode; className?: string }) {
  return (
    <div className={cn("grid min-w-0 gap-1.5", className)}>
      <Label className="text-xs text-muted-foreground">{label}</Label>
      {children}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  )
}

export function CopyField({ label, value }: { label: ReactNode; value: string }) {
  const [copied, setCopied] = useState(false)
  if (!value) return null
  const multiline = value.includes("\n")
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value)
      setCopied(true)
      toast.success("Copied")
      setTimeout(() => setCopied(false), 1500)
    } catch {
      toast.error("Copy failed: select the text and copy it")
    }
  }
  return (
    <Field label={label}>
      <div className="flex items-start gap-2">
        {multiline ? (
          <Textarea readOnly value={value} className="min-h-36 font-mono text-xs" />
        ) : (
          <Input readOnly value={value} className="font-mono text-xs" onFocus={(e) => e.target.select()} />
        )}
        <Button variant="outline" size="icon" onClick={copy} aria-label="Copy">
          {copied ? <CheckIcon /> : <CopyIcon />}
        </Button>
      </div>
    </Field>
  )
}

export function Loading({ rows = 3 }: { rows?: number }) {
  return (
    <div className="space-y-3">
      {Array.from({ length: rows }, (_, i) => (
        <Skeleton key={i} className="h-10 w-full" />
      ))}
    </div>
  )
}

export function ErrorBox({ error, hint }: { error: string; hint?: ReactNode }) {
  return (
    <Alert variant="destructive">
      <AlertTitle>Something went wrong</AlertTitle>
      <AlertDescription>
        <pre className="font-mono text-xs whitespace-pre-wrap break-all">{error}</pre>
        {hint && <p className="mt-2">{hint}</p>}
      </AlertDescription>
    </Alert>
  )
}

export function Stat({ label, value, hint, onClick }: { label: ReactNode; value: ReactNode; hint?: ReactNode; onClick?: () => void }) {
  return (
    <Card className={cn("gap-1 py-4", onClick && "cursor-pointer transition-colors hover:bg-muted/40")} onClick={onClick}>
      <CardContent className="space-y-1 px-4">
        <div className="text-xs text-muted-foreground">{label}</div>
        <div className="text-2xl font-semibold tabular-nums">{value}</div>
        {hint && <div className="text-xs text-muted-foreground">{hint}</div>}
      </CardContent>
    </Card>
  )
}

/**
 * Scrollable middle part of a dialog: the header and footer stay in view. The padding (offset by
 * negative margins) holds the enlarged hit areas of checkboxes and switches (`after:-inset-*`), which
 * would otherwise count as overflow and show a scrollbar on content that fits.
 */
export function DialogBody({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn("-mx-4 -my-2 max-h-[min(65svh,42rem)] overflow-y-auto px-4 py-2", className)}>{children}</div>
}
