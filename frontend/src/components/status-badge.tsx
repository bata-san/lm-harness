import { Badge } from "@/components/ui/badge"
import { cn } from "@/lib/utils"

const styles: Record<string, string> = {
  running: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  completed: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  exited: "bg-muted text-muted-foreground",
  downloading: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  verifying: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
  starting: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  queued: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  paused: "bg-muted text-muted-foreground",
  stopped: "bg-muted text-muted-foreground",
  cancelled: "bg-muted text-muted-foreground",
  failed: "bg-red-500/15 text-red-600 dark:text-red-400",
}

const labels: Record<string, string> = {
  running: "稼働中",
  completed: "完了",
  exited: "終了",
  downloading: "DL中",
  verifying: "検証中",
  starting: "起動中",
  queued: "待機",
  paused: "一時停止",
  stopped: "停止",
  cancelled: "取消",
  failed: "失敗",
}

export function StatusBadge({ status }: { status: string }) {
  return (
    <Badge variant="secondary" className={cn("gap-1.5 border-0", styles[status])}>
      <span
        className={cn(
          "size-1.5 rounded-full bg-current",
          (status === "running" || status === "downloading" || status === "starting" || status === "verifying") && "animate-pulse",
        )}
      />
      {labels[status] ?? status}
    </Badge>
  )
}
