import type { ReactNode } from "react"
import { Loader2Icon, SaveIcon, TriangleAlertIcon, Undo2Icon } from "lucide-react"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"

export function ErrorAlert({ error, title = "読み込みに失敗しました" }: { error: string; title?: string }) {
  return (
    <Alert variant="destructive">
      <TriangleAlertIcon />
      <AlertTitle>{title}</AlertTitle>
      <AlertDescription className="break-all">{error}</AlertDescription>
    </Alert>
  )
}

export function LoadingLines({ rows = 3 }: { rows?: number }) {
  return (
    <div className="grid gap-2" aria-busy="true">
      {Array.from({ length: rows }, (_, i) => (
        <Skeleton key={i} className="h-8 w-full" />
      ))}
    </div>
  )
}

/** Sticky bottom bar shown while a form has unsaved changes. */
export function SaveBar({
  count,
  saving,
  canSave,
  onSave,
  onDiscard,
  children,
}: {
  count: number
  saving: boolean
  canSave: boolean
  onSave: () => void
  onDiscard: () => void
  children?: ReactNode
}) {
  if (count === 0) return null
  return (
    <div className="sticky bottom-3 z-10 mt-4 grid gap-2 rounded-lg border bg-background/95 p-3 shadow-lg backdrop-blur">
      {children}
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium">未保存の変更 {count} 件</span>
        <div className="ml-auto flex gap-2">
          <Button variant="outline" size="sm" disabled={saving} onClick={onDiscard}>
            <Undo2Icon /> 破棄
          </Button>
          <Button size="sm" disabled={saving || !canSave} onClick={onSave}>
            {saving ? <Loader2Icon className="animate-spin" /> : <SaveIcon />}
            保存
          </Button>
        </div>
      </div>
    </div>
  )
}
