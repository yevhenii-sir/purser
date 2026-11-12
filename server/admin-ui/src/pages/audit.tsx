import { useMemo, useState } from "react"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { ErrorBox, Loading, Mono, PageHeader, Section } from "@/components/common"
import { RichTable } from "@/components/rich-table"
import { select } from "@/lib/api"
import { useAsync } from "@/lib/hooks"

interface AuditRow {
  id: number
  at_micros: number
  identity: string
  name: string
  action: string
  target: string
  detail: string
}

const when = (micros: number) => new Date(Number(micros) / 1000).toLocaleString()
const ALL = "__all__"

/** Who changed what: one row per admin action (kept 180 days). */
export function AuditPage() {
  const { data, error, loading } = useAsync(() => select<AuditRow>({ table: "admin_audit" }))
  const [who, setWho] = useState(ALL)
  const [text, setText] = useState("")
  const rows = data || []
  const people = useMemo(() => [...new Map((data || []).map((r) => [r.identity, r.name || r.identity.slice(0, 12) + "…"])).entries()], [data])
  const shown = rows.filter((r) => {
    if (who !== ALL && r.identity !== who) return false
    const q = text.trim().toLowerCase()
    return !q || `${r.action} ${r.target} ${r.detail}`.toLowerCase().includes(q)
  })
  return (
    <>
      <PageHeader title="Audit log" description="Changes made in the admin page and through the admin API, newest first. Secrets are never recorded, only which key changed." />
      <Section>
        {error ? <ErrorBox error={error} /> : loading && !data ? <Loading /> : (
          <RichTable
            rows={shown}
            rowKey={(r) => String(r.id)}
            noun="actions"
            initialSort={{ id: "When", desc: true }}
            empty="No admin actions recorded yet."
            toolbar={
              <div className="flex flex-wrap gap-2">
                <Select value={who} onValueChange={setWho}>
                  <SelectTrigger className="h-8 w-44"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value={ALL}>Everyone</SelectItem>
                    {people.map(([id, label]) => <SelectItem key={id} value={id}>{label}</SelectItem>)}
                  </SelectContent>
                </Select>
                <Input className="h-8 w-56" placeholder="Filter by action, app, key…" value={text} onChange={(e) => setText(e.target.value)} />
              </div>
            }
            columns={[
              { title: "When", sort: (r) => Number(r.at_micros), size: 170, cell: (r) => <span className="text-xs tabular-nums">{when(r.at_micros)}</span> },
              { title: "Who", sort: (r) => r.name, size: 150, cell: (r) => r.name || <Mono>{r.identity.slice(0, 12)}…</Mono> },
              { title: "Action", sort: (r) => r.action, size: 200, cell: (r) => <Mono>{r.action.replace(/^admin_/, "")}</Mono> },
              { title: "Target", sort: (r) => r.target, size: 180, cell: (r) => <span className="block max-w-44 truncate" title={r.target}><Mono>{r.target}</Mono></span> },
              { title: "Detail", size: 420, className: "whitespace-normal", cell: (r) => <span className="line-clamp-3 text-xs break-all text-muted-foreground" title={r.detail}>{r.detail}</span> },
            ]}
          />
        )}
      </Section>
    </>
  )
}
