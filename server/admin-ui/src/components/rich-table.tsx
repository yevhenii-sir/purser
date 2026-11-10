// A sortable, paginated table with resizable and hideable columns for the larger lists (purchases,
// revenue breakdowns, logs). Built on the ReUI data grid (MIT, Keenthemes: components/reui) over
// TanStack Table. Columns are the same `Column<T>` as `DataTable`, plus a `sort` value to sort by.
import { useMemo, useState, type ReactNode } from "react"
import { useTable, type ColumnDef, type PaginationState, type SortingState } from "@tanstack/react-table"
import { Columns3Icon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { DataGrid, DataGridContainer, dataGridFeatures, type DataGridFeatures } from "@/components/reui/data-grid/data-grid"
import { DataGridColumnHeader } from "@/components/reui/data-grid/data-grid-column-header"
import { DataGridColumnVisibility } from "@/components/reui/data-grid/data-grid-column-visibility"
import { DataGridPagination } from "@/components/reui/data-grid/data-grid-pagination"
import { DataGridScrollArea } from "@/components/reui/data-grid/data-grid-scroll-area"
import { DataGridTable } from "@/components/reui/data-grid/data-grid-table"
import type { Column } from "@/components/common"
import { cn } from "@/lib/utils"

export interface RichColumn<T> extends Column<T> {
  /** Stable id (column visibility, sorting); defaults to the title when it is a string. */
  id?: string
  /** Value to sort by; the column is not sortable without it. */
  sort?: (row: T) => string | number | null | undefined
  /** Hidden until switched on in the Columns menu. */
  hidden?: boolean
  /** Initial width in px. */
  size?: number
}

export function RichTable<T extends object>({
  rows,
  columns,
  onRowClick,
  rowKey,
  empty = "Nothing here",
  pageSize = 25,
  initialSort,
  toolbar,
  noun = "rows",
  summary,
}: {
  rows: T[]
  columns: RichColumn<T>[]
  onRowClick?: (row: T) => void
  rowKey?: (row: T, index: number) => string
  empty?: ReactNode
  pageSize?: number
  /** Column id and direction sorted by at first. */
  initialSort?: { id: string; desc?: boolean }
  /** Extra controls on the left of the Columns button. */
  toolbar?: ReactNode
  /** What the rows are, for the count above the table ("purchases"). */
  noun?: string
  /** Replaces the row count above the table. */
  summary?: ReactNode
}) {
  const defs = useMemo<ColumnDef<DataGridFeatures, T>[]>(
    () =>
      columns.map((c, i) => {
        const id = c.id || (typeof c.title === "string" ? c.title : `col${i}`)
        const label = typeof c.title === "string" ? c.title : id
        const cellClass = cn(c.num && "text-right tabular-nums", c.className)
        return {
          id,
          accessorFn: c.sort ? (row: T) => c.sort!(row) ?? (c.num ? -Infinity : "") : () => null,
          enableSorting: !!c.sort,
          enableHiding: i > 0 && c.title !== "",
          sortUndefined: "last",
          size: c.size,
          header: ({ column }) => (
            <DataGridColumnHeader column={column} title={label} className={cn(c.num && "ms-auto")} visibility />
          ),
          cell: ({ row }) => c.cell(row.original, row.index),
          meta: { headerTitle: label, headerClassName: cn(c.num && "text-right"), cellClassName: cellClass },
        } as ColumnDef<DataGridFeatures, T>
      }),
    [columns],
  )
  const [sorting, setSorting] = useState<SortingState>(initialSort ? [{ id: initialSort.id, desc: !!initialSort.desc }] : [])
  const [pagination, setPagination] = useState<PaginationState>({ pageIndex: 0, pageSize })
  const [columnVisibility, setColumnVisibility] = useState<Record<string, boolean>>(() =>
    Object.fromEntries(columns.filter((c) => c.hidden).map((c, i) => [c.id || (typeof c.title === "string" ? c.title : `col${i}`), false])),
  )
  const table = useTable({
    features: dataGridFeatures,
    columns: defs,
    data: rows,
    getRowId: rowKey ? (row: T, index: number) => rowKey(row, index) : undefined,
    state: { sorting, pagination, columnVisibility },
    onSortingChange: setSorting,
    onPaginationChange: setPagination,
    onColumnVisibilityChange: setColumnVisibility,
    autoResetPageIndex: true,
  })

  if (!rows.length) return <div className="py-8 text-center text-sm text-muted-foreground">{empty}</div>
  const paged = rows.length > pagination.pageSize
  return (
    <DataGrid
      table={table}
      recordCount={rows.length}
      onRowClick={onRowClick}
      emptyMessage={empty}
      tableLayout={{ dense: true, headerSticky: true, columnsResizable: true, columnsVisibility: true, width: "auto" }}
      tableClassNames={{ bodyRow: cn(onRowClick && "cursor-pointer") }}
    >
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <div className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
            {summary ?? `${rows.length.toLocaleString()} ${noun}`}
          </div>
          {toolbar}
          <DataGridColumnVisibility
            table={table}
            trigger={<Button variant="ghost" size="sm" className="text-muted-foreground"><Columns3Icon /> Columns</Button>}
          />
        </div>
        <DataGridContainer>
          <DataGridScrollArea>
            <DataGridTable />
          </DataGridScrollArea>
        </DataGridContainer>
        {paged && <DataGridPagination sizes={[10, 25, 50, 100]} />}
      </div>
    </DataGrid>
  )
}
