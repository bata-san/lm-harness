import { useEffect, useRef, useState } from "react"
import { CircleCheckIcon, PlayIcon, XIcon } from "lucide-react"

import { ErrorAlert, LoadingLines } from "@/components/admin/shared"
import type { ActionInfo, ActionResult, LogsResponse } from "@/components/admin/types"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { ApiError, api, usePoll } from "@/lib/api"
import { cn } from "@/lib/utils"

type Outcome = { ok: boolean; message: string }

export function ActionsTab({ jobId, onJob }: { jobId: string | null; onJob: (id: string | null) => void }) {
  const { data, error } = usePoll<{ actions: ActionInfo[] }>("/admin/actions", 0)
  const [target, setTarget] = useState<ActionInfo | null>(null)
  const [outcome, setOutcome] = useState<Outcome | null>(null)

  async function run(a: ActionInfo) {
    try {
      const r = await api<ActionResult>(`/admin/actions/${encodeURIComponent(a.id)}`, { method: "POST" })
      setOutcome({ ok: r.ok, message: r.message })
      if (r.job_id) onJob(r.job_id)
    } catch (e) {
      // 409 = the action was refused or failed on the server; show it as an error result, not a toast.
      if (e instanceof ApiError && e.status === 409) {
        setOutcome({ ok: false, message: e.message })
        return
      }
      throw e
    }
  }

  return (
    <div className="grid gap-4">
      <Card>
        <CardHeader>
          <CardTitle>メンテナンス操作</CardTitle>
          <CardDescription>許可された操作だけを実行できます。実行前に確認が出ます。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-3">
          {error && <ErrorAlert error={error} />}
          {!data && !error && <LoadingLines rows={4} />}
          {data?.actions.map((a) => (
            <div key={a.id} className="flex flex-col gap-2 rounded-md border p-3 sm:flex-row sm:items-center">
              <div className="min-w-0 flex-1">
                <code className="font-mono text-sm font-medium break-all">{a.id}</code>
                <p className="mt-0.5 text-sm text-muted-foreground break-words">{a.description}</p>
              </div>
              <Button variant="outline" size="sm" className="shrink-0 self-start sm:self-center" onClick={() => setTarget(a)}>
                <PlayIcon /> 実行
              </Button>
            </div>
          ))}
        </CardContent>
      </Card>

      {outcome && (
        <Alert variant={outcome.ok ? "default" : "destructive"}>
          {outcome.ok ? <CircleCheckIcon /> : <XIcon />}
          <AlertTitle>{outcome.ok ? "実行しました" : "実行できませんでした"}</AlertTitle>
          <AlertDescription className="break-words whitespace-pre-wrap">{outcome.message}</AlertDescription>
        </Alert>
      )}

      {jobId && <JobLog jobId={jobId} onClose={() => onJob(null)} />}

      <ConfirmDialog
        open={target !== null}
        onOpenChange={(o) => !o && setTarget(null)}
        title={target ? `「${target.id}」を実行しますか` : ""}
        description={target?.description}
        confirmLabel="実行"
        destructive={target?.id !== "drop-caches-hint"}
        onConfirm={() => (target ? run(target) : undefined)}
      />
    </div>
  )
}

/** Live output of a maintenance job, polled every 2 seconds until closed. */
function JobLog({ jobId, onClose }: { jobId: string; onClose: () => void }) {
  const { data, error } = usePoll<LogsResponse>(`/admin/logs?source=${encodeURIComponent(`job:${jobId}`)}&lines=300`, 2000)
  const box = useRef<HTMLPreElement>(null)

  useEffect(() => {
    if (box.current) box.current.scrollTop = box.current.scrollHeight
  }, [data])

  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-2">
          <div className="min-w-0">
            <CardTitle>ジョブのログ</CardTitle>
            <CardDescription className="font-mono break-all">job:{jobId} · 2 秒ごとに更新</CardDescription>
          </div>
          <Button variant="ghost" size="sm" className="shrink-0" onClick={onClose}>
            <XIcon /> 閉じる
          </Button>
        </div>
      </CardHeader>
      <CardContent>
        {error && <ErrorAlert error={error} title="ログを取得できません" />}
        <pre
          ref={box}
          className={cn(
            "h-80 overflow-auto rounded-lg border bg-zinc-950 p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap break-all text-zinc-200",
          )}
        >
          {data ? (data.lines.length ? data.lines.join("\n") : "ログはまだありません…") : "読み込み中…"}
        </pre>
      </CardContent>
    </Card>
  )
}
