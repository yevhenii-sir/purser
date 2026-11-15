import { useState } from "react"
import { CloudIcon, WandSparklesIcon } from "lucide-react"
import { toast } from "sonner"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { DialogBody, ErrorBox, Field, Mono, Tone } from "@/components/common"
import { SecretField } from "@/components/secret-field"
import { genSecret, proc } from "@/lib/api"
import { errorText } from "@/lib/format"
import { tryAct } from "@/lib/hooks"
import { storeOf } from "@/lib/stores"
import type { GameApp } from "@/lib/types"

type Done = { ok: boolean; detail?: string; action?: string; endpoint?: string }

/** One-click store setup of a build whose store key is in place: App Store notification URLs or the
 * Stripe webhook endpoint. Nothing for other stores. */
export function StoreSetupButton({ app, base, onDone }: { app: GameApp; base: string; onDone: () => void }) {
  const [busy, setBusy] = useState(false)
  const store = storeOf(app).id
  if (store !== "apple" && store !== "stripe") return null
  const run = async () => {
    setBusy(true)
    const done = await tryAct(async () => {
      if (store === "apple") {
        const r = await proc<Done>("admin_setup_apple_notifications", app.app_id, base)
        return r.ok ? "App Store Connect sends notifications here" : `Saved, but: ${r.detail}`
      }
      try {
        const r = await proc<Done>("admin_setup_stripe_webhook", app.app_id, base, false)
        return `Stripe endpoint ${r.endpoint} ${r.action}`
      } catch (e) {
        if (!errorText(e).includes("ERR_STRIPE_WEBHOOK_EXISTS")) throw e
        if (!confirm("Stripe already has an endpoint for this build, but its signing secret is not stored here. Replace it? Stripe stops sending to the old one.")) return null
        const r = await proc<Done>("admin_setup_stripe_webhook", app.app_id, base, true)
        return `Stripe endpoint ${r.endpoint} ${r.action}`
      }
    })
    setBusy(false)
    if (done) {
      toast.success(done)
      onDone()
    }
  }
  const [label, title] = store === "apple"
    ? ["Set in App Store Connect", "Points the App Store Server Notifications URLs (production and sandbox, version 2) to this server. Needs the App Store Connect Team key."]
    : ["Create webhook in Stripe", "Creates the webhook endpoint of this build in Stripe with the events Purser handles and stores its signing secret."]
  return (
    <Button size="xs" variant="outline" disabled={busy || !base} title={title} onClick={run}>
      <WandSparklesIcon className={busy ? "animate-pulse" : ""} /> {busy ? "Working…" : label}
    </Button>
  )
}

type Step = { step: string; action: string; detail: string }
type GoogleResult = { ok: boolean; dry_run: boolean; service_account: string; scope: string; topic: string; steps: Step[]; manual: string[] }

const TONE: Record<string, "ok" | "warn" | "bad" | "muted"> = {
  created: "ok", updated: "ok", exists: "muted", pending: "warn", failed: "bad",
}

/** Google Cloud side of Google Play, run with a one-time access token of the project's owner. */
export function GoogleSetupDialog({ apps, base, open, onClose }: { apps: GameApp[]; base: string; open: boolean; onClose: (changed: boolean) => void }) {
  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose(false)}>
      <DialogContent className="sm:max-w-2xl">{open && <GoogleSetup apps={apps} base={base} onClose={onClose} />}</DialogContent>
    </Dialog>
  )
}

function GoogleSetup({ apps, base, onClose }: { apps: GameApp[]; base: string; onClose: (changed: boolean) => void }) {
  const [project, setProject] = useState("")
  const [token, setToken] = useState("")
  const [topic, setTopic] = useState("play-billing")
  const [newKey, setNewKey] = useState(false)
  const [rotate, setRotate] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<GoogleResult | null>(null)
  const [changed, setChanged] = useState(false)

  const run = async (dryRun: boolean) => {
    setBusy(true)
    setError(null)
    try {
      const r = await proc<GoogleResult>("admin_setup_google", JSON.stringify({
        project: project.trim(), app_ids: apps.map((a) => a.app_id), access_token: token.trim(), topic: topic.trim(),
        route: base, new_key: newKey, rotate_rtdn_secret: rotate, dry_run: dryRun, entropy: genSecret(),
      }))
      setResult(r)
      if (!dryRun) setChanged(true)
    } catch (e) {
      setError(errorText(e))
    }
    setBusy(false)
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>Set up with Google Cloud</DialogTitle>
        <DialogDescription>
          Creates what Google Play needs in your Cloud project: the APIs, a service account and its key, the Pub/Sub topic of
          real-time notifications and a push subscription to this server for {apps.length === 1 ? "this build" : `these ${apps.length} builds`}.
          Every step looks first, so running it again is safe.
        </DialogDescription>
      </DialogHeader>
      <DialogBody className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Project id" hint="The Google Cloud project of your Play Console developer account.">
            <Input value={project} placeholder="my-game-123456" spellCheck={false} className="font-mono text-xs" onChange={(e) => setProject(e.target.value)} />
          </Field>
          <Field label="Pub/Sub topic">
            <Input value={topic} spellCheck={false} className="font-mono text-xs" onChange={(e) => setTopic(e.target.value)} />
          </Field>
        </div>
        <Field label="Access token" hint={<>Run <Mono>gcloud auth print-access-token</Mono> as the project's owner and paste the output. It lasts an hour, is used for this setup only and is not stored.</>}>
          <SecretField value={token} onChange={setToken} placeholder="ya29.…" />
        </Field>
        <div className="flex flex-wrap gap-x-6 gap-y-2">
          <Label className="flex items-center gap-2 text-sm font-normal">
            <Checkbox checked={newKey} onCheckedChange={(v) => setNewKey(v === true)} /> Create a new key even if one is stored
          </Label>
          <Label className="flex items-center gap-2 text-sm font-normal">
            <Checkbox checked={rotate} onCheckedChange={(v) => setRotate(v === true)} /> New RTDN secrets
          </Label>
        </div>
        {error && <ErrorBox error={error} />}
        {result && (
          <div className="space-y-3">
            <ul className="space-y-1.5 text-sm">
              {result.steps.map((s) => (
                <li key={s.step} className="flex min-w-0 items-baseline gap-2">
                  <Tone tone={TONE[s.action] ?? "muted"}>{s.action.replace("_", " ")}</Tone>
                  <span className="shrink-0 font-medium">{s.step.replace("_", " ")}</span>
                  <span className="min-w-0 truncate text-xs text-muted-foreground" title={s.detail}>{s.detail}</span>
                </li>
              ))}
            </ul>
            {!result.dry_run && (
              <div className="rounded-md border p-3 text-sm">
                <p className="mb-2 font-medium">Still to do by hand (no API for these):</p>
                <ol className="list-decimal space-y-1 pl-5 text-muted-foreground">
                  {result.manual.map((m) => <li key={m}>{m}</li>)}
                </ol>
              </div>
            )}
          </div>
        )}
      </DialogBody>
      <DialogFooter>
        <Button variant="outline" onClick={() => onClose(changed)}>{changed ? "Done" : "Cancel"}</Button>
        <Button variant="outline" disabled={busy || !project || !token} onClick={() => run(true)}>Preview</Button>
        <Button disabled={busy || !project || !token} onClick={() => run(false)}>
          <CloudIcon className={busy ? "animate-pulse" : ""} /> {busy ? "Working…" : "Set up"}
        </Button>
      </DialogFooter>
    </>
  )
}
