import { useCallback, useEffect, useRef, useState } from "react"
import { toast } from "sonner"
import { errorText } from "./format"

export interface LoadOptions {
  /** True when the load was started by `reload()`: skip caches. */
  fresh: boolean
}

/** Loads data on mount and when `deps` change; `reload()` fetches again (bypassing caches).
 * Responses of an earlier load that arrive after a newer one started are ignored. */
export function useAsync<T>(load: (opts: LoadOptions) => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T | undefined>(undefined)
  const [error, setError] = useState<string>("")
  const [loading, setLoading] = useState(true)
  const [tick, setTick] = useState(0)
  const fresh = useRef(false)

  useEffect(() => {
    let alive = true
    const opts = { fresh: fresh.current }
    fresh.current = false
    setLoading(true)
    load(opts)
      .then((d) => {
        if (alive) {
          setData(d)
          setError("")
        }
      })
      .catch((e) => alive && setError(errorText(e)))
      .finally(() => alive && setLoading(false))
    return () => {
      alive = false
    }
    // `deps` stand for `load`, a new closure on every render. oxlint reports this rule (react-hooks).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick])

  const reload = useCallback(() => {
    fresh.current = true
    setTick((t) => t + 1)
  }, [])
  return { data, error, loading, reload }
}

const memo = new Map<string, { at: number; value: Promise<unknown> }>()

/** Session cache of expensive reads: the same `key` within `ttlMs` shares one request (also one
 * still in flight). `fresh` loads again. Failed loads are not kept. */
export function cached<T>(key: string, ttlMs: number, load: () => Promise<T>, fresh = false): Promise<T> {
  const hit = memo.get(key)
  if (!fresh && hit && Date.now() - hit.at < ttlMs) return hit.value as Promise<T>
  const value = load()
  memo.set(key, { at: Date.now(), value })
  value.catch(() => {
    if (memo.get(key)?.value === value) memo.delete(key)
  })
  return value
}

/** Drops cached entries whose key starts with `prefix`. */
export function forget(prefix: string) {
  for (const k of [...memo.keys()]) if (k.startsWith(prefix)) memo.delete(k)
}

/** Runs an action, shows `ok` as a toast on success and the error otherwise. Rethrows. */
export async function act<T>(fn: () => Promise<T>, ok?: string): Promise<T> {
  try {
    const r = await fn()
    if (ok) toast.success(ok)
    return r
  } catch (e) {
    toast.error(errorText(e))
    throw e
  }
}

/** `copy(text, message?)` puts text on the clipboard and toasts `message` (a failure is toasted too);
 * `copied` is true for a moment after. */
export function useCopy(success?: string) {
  const [copied, setCopied] = useState(false)
  const copy = useCallback(async (text: string, message = success) => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      if (message) toast.success(message)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      toast.error("Copy failed: select the text and copy it")
    }
  }, [success])
  return { copied, copy }
}

/** Like `act`, but swallows the error after showing it (for click handlers). */
export function tryAct<T>(fn: () => Promise<T>, ok?: string): Promise<T | undefined> {
  return act(fn, ok).catch(() => undefined)
}
