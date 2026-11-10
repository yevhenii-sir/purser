import { useState } from "react"
import { Trash2Icon, UserPlusIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { DataTable, ErrorBox, Loading, Mono, PageHeader, Section, Tone } from "@/components/common"
import { call, sql } from "@/lib/api"
import { tryAct, useAsync } from "@/lib/hooks"
import type { PageProps } from "@/routes"

const hex = (id: unknown) => String(id).replace(/^0x/, "").toLowerCase()

export function AdminsPage({ me }: PageProps) {
  const [identity, setIdentity] = useState("")
  const { data, error, loading, reload } = useAsync(() => sql<{ identity: string; protected: boolean }>("SELECT * FROM admin"))
  return (
    <>
      <PageHeader
        title="Admins"
        description="Admins can change the configuration. Only the database owner can read private tables, and therefore use this page. The owner cannot be removed."
      />
      <Section>
        {error ? <ErrorBox error={error} /> : loading || !data ? <Loading /> : (
          <DataTable
            rows={data}
            columns={[
              {
                title: "Identity",
                className: "whitespace-normal",
                cell: (r) => (
                  <span className="flex flex-wrap items-center gap-2">
                    <Mono className="break-all">{hex(r.identity)}</Mono>
                    {hex(r.identity) === me && <Tone tone="ok">you</Tone>}
                    {r.protected && <Tone tone="muted">owner</Tone>}
                  </span>
                ),
              },
              {
                title: "",
                className: "text-right",
                cell: (r) => !r.protected && hex(r.identity) !== me && (
                  <Button variant="ghost" size="icon-sm" className="text-destructive" aria-label="Remove" onClick={async () => {
                    if (!confirm("Remove this admin?")) return
                    await tryAct(() => call("admin_remove_admin", hex(r.identity)), "Removed")
                    reload()
                  }}><Trash2Icon /></Button>
                ),
              },
            ]}
          />
        )}
        <div className="mt-4 flex gap-2">
          <Input className="font-mono text-xs" placeholder="identity hex (64 chars)" value={identity} onChange={(e) => setIdentity(e.target.value)} />
          <Button onClick={async () => { await tryAct(() => call("admin_add_admin", hex(identity.trim())), "Added"); setIdentity(""); reload() }}>
            <UserPlusIcon /> Add
          </Button>
        </div>
      </Section>
    </>
  )
}
