export { cn } from "cn"

/** `text` parsed as JSON, or `fallback` when it is empty or not JSON. */
export function parseJsonOr<T>(text: string | null | undefined, fallback: T): T {
  if (!text) return fallback
  try {
    return JSON.parse(text) as T
  } catch {
    return fallback
  }
}

/** Saves `text` as a file named `name`. */
export function downloadBlob(name: string, text: string, type: string) {
  const url = URL.createObjectURL(new Blob([text], { type }))
  Object.assign(document.createElement("a"), { href: url, download: name }).click()
  // Revoking right after click() can cancel the download in some browsers.
  setTimeout(() => URL.revokeObjectURL(url), 0)
}
