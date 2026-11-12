import { DataTable, LevelBadge, Mono } from "@/components/common"
import { fmtMs } from "@/lib/format"
import type { Report } from "@/lib/types"

export function ReportView({ report }: { report: Report }) {
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <LevelBadge level={report.summary} />
        <Mono className="font-medium">{report.app_id}</Mono>
        <span className="text-muted-foreground">checked {fmtMs(report.checked_at_ms)}</span>
      </div>
      <DataTable
        rows={report.checks}
        columns={[
          { title: "", cell: (c) => <LevelBadge level={c.level} />, className: "w-16" },
          { title: "Check", cell: (c) => <span className="font-medium whitespace-nowrap">{c.title}</span> },
          {
            title: "Result",
            cell: (c) => (
              <div className="space-y-1 whitespace-normal">
                <div className="break-words">{c.detail}</div>
                {c.fix && (c.level === "warn" || c.level === "fail") && <div className="text-xs text-muted-foreground">{c.fix}</div>}
              </div>
            ),
          },
        ]}
      />
    </div>
  )
}
