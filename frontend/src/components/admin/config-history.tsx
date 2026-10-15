import { Undo2Icon } from "lucide-react"

import type { ConfigHistoryEntry } from "@/components/admin/types"
import { Button } from "@/components/ui/button"
import { bytes } from "@/lib/format"

/** Newest-first list of saved config snapshots, each with a rollback button. */
export function ConfigHistory({
  history,
  onRollback,
  format,
}: {
  history: ConfigHistoryEntry[]
  onRollback: (version: string) => void
  format: (h: ConfigHistoryEntry) => string
}) {
  if (history.length === 0) {
    return <p className="text-sm text-muted-foreground">履歴はまだありません。設定を保存すると記録されます。</p>
  }
  return (
    <ul className="grid max-h-[32rem] divide-y overflow-y-auto">
      {history.map((h) => (
        <li key={h.version} className="flex items-center gap-2 py-2">
          <div className="min-w-0 flex-1">
            <div className="text-sm font-medium tabular-nums">{format(h)}</div>
            <div className="truncate font-mono text-[11px] text-muted-foreground">
              {h.version} · {bytes(h.size)}
            </div>
          </div>
          <Button variant="outline" size="sm" onClick={() => onRollback(h.version)}>
            <Undo2Icon /> ロールバック
          </Button>
        </li>
      ))}
    </ul>
  )
}
