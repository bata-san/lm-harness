import { Progress } from "@/components/ui/progress"
import { cn } from "@/lib/utils"

export function Meter({ label, value, detail, tone }: { label: string; value: number; detail?: string; tone?: "warn" | "danger" }) {
  const auto = value >= 90 ? "danger" : value >= 75 ? "warn" : undefined
  const t = tone ?? auto
  return (
    <div className="space-y-1.5">
      <div className="flex items-baseline justify-between text-xs">
        <span className="text-muted-foreground">{label}</span>
        <span className="font-medium tabular-nums">{detail ?? `${value.toFixed(0)}%`}</span>
      </div>
      <Progress
        value={value}
        className={cn(
          "[&_[data-slot=progress-track]]:h-2",
          t === "warn" && "[&_[data-slot=progress-indicator]]:bg-amber-500",
          t === "danger" && "[&_[data-slot=progress-indicator]]:bg-red-500",
        )}
      />
    </div>
  )
}

export function KV({ k, v, className }: { k: string; v: React.ReactNode; className?: string }) {
  return (
    <div className={cn("flex flex-col", className)}>
      <span className="text-[11px] text-muted-foreground">{k}</span>
      <span className="text-sm font-medium tabular-nums">{v}</span>
    </div>
  )
}
