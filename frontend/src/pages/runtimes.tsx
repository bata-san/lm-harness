import { useState } from "react"
import { CheckCircle2Icon, CircleDashedIcon, CopyIcon, PlayIcon, ScrollTextIcon, SquareIcon, TerminalIcon } from "lucide-react"
import { toast } from "sonner"

import { LogViewer } from "@/components/log-viewer"
import { StatusBadge } from "@/components/status-badge"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { api, usePoll, type Job, type JobRun, type Runtime } from "@/lib/api"
import { since } from "@/lib/format"

export function RuntimesPage() {
  const { data: runtimes, refetch } = usePoll<Runtime[]>("/runtimes", 15000)
  const { data: jobs, refetch: refetchJobs } = usePoll<{ available: Job[]; runs: JobRun[] }>("/jobs", 2000)
  const [logRun, setLogRun] = useState<JobRun | null>(null)

  const jobById = Object.fromEntries((jobs?.available ?? []).map((j) => [j.id, j]))

  async function run(job: Job) {
    if (job.sudo) {
      navigator.clipboard.writeText(job.command)
      toast.info("sudo が必要です。WSL のターミナルで実行してください (コマンドをコピーしました)", { description: job.command })
      return
    }
    try {
      const r = await api<JobRun>(`/jobs/${job.id}`, { method: "POST" })
      refetchJobs()
      setLogRun(r)
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  async function stop(r: JobRun) {
    await api(`/jobs/runs/${r.id}/stop`, { method: "POST" })
    refetchJobs()
  }

  return (
    <div className="grid gap-4">
      <p className="text-sm text-muted-foreground">
        Strata がメインエンジン (既定) です。llama.cpp / vLLM / Ollama は補助ランタイムとして残しています。
      </p>
      <div className="grid gap-4 md:grid-cols-3">
        {(runtimes ?? []).map((rt) => {
          const job = jobById[rt.install_job]
          return (
            <Card key={rt.id}>
              <CardHeader>
                <CardTitle className="flex items-center gap-2">
                  {rt.installed ? <CheckCircle2Icon className="size-4 text-emerald-500" /> : <CircleDashedIcon className="size-4 text-muted-foreground" />}
                  {rt.id}
                  {rt.running && <Badge className="bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">service 稼働中</Badge>}
                </CardTitle>
                <CardDescription>{rt.description}</CardDescription>
              </CardHeader>
              <CardContent className="grid gap-1 text-sm">
                <div>
                  <span className="text-muted-foreground">バージョン: </span>
                  {rt.version ?? "未インストール"}
                </div>
                {rt.path && <div className="truncate font-mono text-xs text-muted-foreground">{rt.path}</div>}
              </CardContent>
              {job && (
                <CardFooter>
                  <Button
                    variant={rt.installed ? "outline" : "default"}
                    className="w-full"
                    onClick={async () => {
                      await run(job)
                      setTimeout(refetch, 2000)
                    }}
                  >
                    {job.sudo ? <TerminalIcon /> : <PlayIcon />}
                    {rt.installed ? "更新 / 再ビルド" : "インストール"}
                  </Button>
                </CardFooter>
              )}
            </Card>
          )
        })}
      </div>

      <Card>
        <CardHeader>
          <CardTitle>セットアップジョブ</CardTitle>
          <CardDescription>scripts/wsl/*.sh をハーネスから実行します。sudo が必要なものはコマンドをコピーしてターミナルで実行してください。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-2">
          {(jobs?.available ?? []).map((j) => (
            <div key={j.id} className="flex flex-wrap items-center gap-2 rounded-lg border p-2 pl-3">
              <span className="font-medium">{j.title}</span>
              {j.sudo && <Badge variant="outline">sudo</Badge>}
              <code className="font-mono text-xs text-muted-foreground">{j.command}</code>
              <div className="ml-auto flex gap-1">
                <Button
                  size="icon-sm"
                  variant="ghost"
                  title="コマンドをコピー"
                  onClick={() => {
                    navigator.clipboard.writeText(j.command)
                    toast.success("コピーしました")
                  }}
                >
                  <CopyIcon />
                </Button>
                {!j.sudo && (
                  <Button size="icon-sm" variant="ghost" title="実行" onClick={() => run(j)}>
                    <PlayIcon />
                  </Button>
                )}
              </div>
            </div>
          ))}
        </CardContent>
      </Card>

      {(jobs?.runs ?? []).length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>実行履歴</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-2">
            {jobs!.runs.map((r) => (
              <div key={r.id} className="flex items-center gap-2 rounded-lg border p-2 pl-3 text-sm">
                <span>{r.name}</span>
                <StatusBadge status={r.status} />
                <span className="text-xs text-muted-foreground">{since(r.started_at)} 前</span>
                <div className="ml-auto flex gap-1">
                  <Button size="icon-sm" variant="ghost" title="ログ" onClick={() => setLogRun(r)}>
                    <ScrollTextIcon />
                  </Button>
                  {(r.status === "starting" || r.status === "running") && (
                    <Button size="icon-sm" variant="ghost" title="中断" onClick={() => stop(r)}>
                      <SquareIcon />
                    </Button>
                  )}
                </div>
              </div>
            ))}
          </CardContent>
        </Card>
      )}

      <Dialog open={!!logRun} onOpenChange={(o) => !o && setLogRun(null)}>
        <DialogContent className="sm:max-w-5xl">
          <DialogHeader>
            <DialogTitle>{logRun?.name}</DialogTitle>
          </DialogHeader>
          {logRun && <LogViewer path={`/jobs/runs/${logRun.id}/logs`} className="h-[65vh]" />}
        </DialogContent>
      </Dialog>
    </div>
  )
}
