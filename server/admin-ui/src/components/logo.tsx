import { cn } from "@/lib/utils"

/** The Purser mark: a ledger-striped P on a rounded tile (docs/images/purser.svg), in the theme's colors. */
export function Logo({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 120 120" aria-hidden="true" className={cn("size-8 shrink-0", className)}>
      <rect width="120" height="120" rx="26" className="fill-foreground" />
      <path d="M38 32Q38 28 42 28H66A22 22 0 0 1 66 72H56V87A9 9 0 0 1 38 87Z" className="fill-background" />
      <path d="M36 42H90M36 56H90M36 70H90M36 84H90" strokeWidth="4" className="stroke-foreground" />
    </svg>
  )
}
