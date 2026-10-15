import { useEffect, useState } from "react"
import { CopyIcon, ExternalLinkIcon, GaugeIcon, Loader2Icon, LayersIcon, PlayIcon, PauseIcon, RefreshCwIcon, ScrollTextIcon, SquareIcon, TimerIcon, WrenchIcon, ZapIcon } from "lucide-react"
import { toast } from "sonner"

import { ErrorAlert } from "@/components/admin/shared"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { KV, Meter } from "@/components/stat"
import { StatusBadge } from "@/components/status-badge"
import { ModeSwitch } from "@/components/mode-switch"
import { LogViewer } from "@/components/log-viewer"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import { ApiError, api, usePoll, type Instance, type Job, type JobRun, type Runtime, type StrataStatus, type SystemSnapshot } from "@/lib/api"
import { copyText } from "@/lib/clipboard"
import { bytes, pct } from "@/lib/format"
import { gb, profileSummary, useMode } from "@/lib/mode"
import { STRATA_STATE_LABEL, countdownLabel, deriveStrataState, idleCountdown, strataActions, strataWarnings, type StrataState } from "@/lib/strata"
import { cn } from "@/lib/utils"

const CHIP_STYLE: Record<StrataState, string> = {
  stopped: "bg-muted text-muted-foreground",
  starting: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  loaded: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  unloaded: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  error: "bg-red-500/15 text-red-600 dark:text-red-400",
}

const DONE_MESSAGE = {
  start: "Strata を起動しました。ロード完了まで時間がかかることがあります",
  load: "モデルをロードしました",
  unload: "アンロードしました。VRAM を解放しています",
} as const

const API_BASE = [
  { label: "OpenAI 互換", path: "/v1" },
  { label: "Anthropic Messages", path: "/v1/messages" },
  { label: "OpenAI Responses", path: "/v1/responses" },
]

export function StrataPage() {
  const { data: status, error: statusError, refetch: refetchStatus } = usePoll<StrataStatus>("/strata/status", 2000)
  const { data: runtimes } = usePoll<Runtime[]>("/runtimes", 15000)
  const { data: sys } = usePoll<SystemSnapshot>("/system", 2000)
  const { data: instances, refetch: refetchInstances } = usePoll<Instance[]>("/instances", 2000)
  const { data: jobs, refetch: refetchJobs } = usePoll<{ available: Job[]; runs: JobRun[] }>("/jobs", 2000)
  const { state: modeState, mode: modeName } = useMode()

  const [pick, setPick] = useState("")
  const [busy, setBusy] = useState<keyof typeof DONE_MESSAGE | "stop" | null>(null)
  const [stopOpen, setStopOpen] = useState(false)
  const [setupOpen, setSetupOpen] = useState(false)
  const [jobLog, setJobLog] = useState<JobRun | null>(null)

  // Installed = the runtime entry says so; without it, fall back to what the status reports.
  const strataRt = (runtimes ?? []).find((r) => r.id === "strata")
  const installed = status ? (strataRt ? strataRt.installed : status.configs.length > 0 || status.running) : true
  const configs = status?.configs ?? []
  const noConfigs = configs.length === 0
  const config = configs.includes(pick) ? pick : (configs[0] ?? "")
  const state = deriveStrataState(status)
  const actions = strataActions(status, config)
  const warnings = strataWarnings(status)
  const inst = status?.instance ? instances?.find((i) => i.id === status.instance?.id) : undefined
  const gpu = sys?.gpus[0]

  const jobById = Object.fromEntries((jobs?.available ?? []).map((j) => [j.id, j]))
  const jobRun = jobLog ? (jobs?.runs ?? []).find((r) => r.id === jobLog.id) : undefined
  const jobRunning = jobRun?.status === "starting" || jobRun?.status === "running"

  async function act(key: keyof typeof DONE_MESSAGE, path: string, body?: unknown) {
    setBusy(key)
    try {
      await api(path, { method: "POST", json: body })
      toast.success(DONE_MESSAGE[key])
    } catch (e) {
      if (key === "unload" && e instanceof ApiError && e.status === 409) {
        toast.error("処理中のため解除できません", { description: e.message })
      } else {
        toast.error((e as Error).message)
      }
    } finally {
      setBusy(null)
      refetchStatus()
    }
  }

  async function runJob(job: Job) {
    if (job.sudo) {
      navigator.clipboard?.writeText(job.command).catch(() => {})
      toast.info("sudo が必要です。WSL のターミナルで実行してください (コマンドをコピーしました)", { description: job.command })
      return
    }
    try {
      const r = await api<JobRun>(`/jobs/${job.id}`, { method: "POST" })
      setJobLog(r)
      refetchJobs()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  async function stopJob() {
    if (!jobLog) return
    try {
      await api(`/jobs/runs/${jobLog.id}/stop`, { method: "POST" })
      refetchJobs()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  const update = jobById["update-strata"]
  const bench = jobById["bench-strata"]
  const origin = window.location.origin

  return (
    <div className="grid min-w-0 gap-4">
      {warnings.length > 0 && (
        <Alert variant="destructive">
          <AlertTitle>注意が必要です</AlertTitle>
          <AlertDescription className="grid gap-1 break-words">
            {warnings.map((w) => (
              <span key={w}>{w}</span>
            ))}
          </AlertDescription>
        </Alert>
      )}
      {statusError && !status && <ErrorAlert error={statusError} title="Strata の状態を取得できません" />}

      {!status ? (
        <Skeleton className="h-56" />
      ) : !installed ? (
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <LayersIcon className="size-4 text-muted-foreground" /> Strata が未インストールです
            </CardTitle>
            <CardDescription>
              Strata は MoE モデルの専門家 (expert) を VRAM / RAM / SSD に自動配置して動かす既定のメインエンジンです。導入すると、このページから起動・ロード・監視ができます。
            </CardDescription>
          </CardHeader>
          <CardContent className="grid gap-3">
            <SetupHint />
            <div>
              <Button variant="outline" nativeButton={false} render={<a href="#runtimes" />}>
                <WrenchIcon /> ランタイムを開く
              </Button>
            </div>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardHeader>
            <CardTitle className="flex flex-wrap items-center gap-2">
              <LayersIcon className="size-4 text-emerald-500" /> Strata
              <Badge className={cn("border-0", CHIP_STYLE[state])}>{STRATA_STATE_LABEL[state]}</Badge>
              {status.instance && <Badge variant="outline" className="max-w-full truncate font-mono">{status.instance.config}</Badge>}
            </CardTitle>
            <CardDescription>
              {status.running && status.instance ? `${status.instance.name} · ポート ${status.instance.port}` : "停止中です。設定を選んで起動してください"}
            </CardDescription>
          </CardHeader>
          <CardContent className="grid gap-4">
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              <KV k="生成 tok/s" v={inst?.metrics.gen_tps?.toFixed(1) ?? "—"} />
              <KV k="キュー" v={inst?.metrics.deferred ?? "—"} />
              <KV k="処理中" v={inst?.metrics.processing ?? "—"} />
              <KV k="プロンプト tok/s" v={inst?.metrics.prompt_tps?.toFixed(0) ?? "—"} />
            </div>

            <div className="grid gap-3 md:grid-cols-2">
              {gpu ? (
                <Meter label={`VRAM · ${gpu.name}`} value={pct(gpu.memory_used_mb, gpu.memory_total_mb)} detail={`${bytes(gpu.memory_used_mb * 2 ** 20)} / ${bytes(gpu.memory_total_mb * 2 ** 20)}`} />
              ) : (
                <p className="text-sm text-muted-foreground">GPU 情報がありません (nvidia-smi 未検出)</p>
              )}
              {sys ? (
                <Meter label="RAM" value={sys.memory.percent} detail={`${bytes(sys.memory.used)} / ${bytes(sys.memory.total)}`} />
              ) : (
                <Skeleton className="h-8" />
              )}
            </div>

            <IdleCountdown status={status} />

            <div className="grid gap-2 border-t pt-4">
              <div className="flex flex-wrap items-center gap-2">
                <Select value={config} onValueChange={(v) => setPick(String(v))} disabled={status.running || noConfigs}>
                  <SelectTrigger className="w-full sm:w-64" aria-label="設定">
                    <SelectValue placeholder="設定がありません" />
                  </SelectTrigger>
                  <SelectContent>
                    {configs.map((c) => (
                      <SelectItem key={c} value={c}>
                        {c}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button disabled={!actions.start || busy !== null} onClick={() => act("start", "/strata/start", { config })}>
                  {busy === "start" ? <Loader2Icon className="animate-spin" /> : <PlayIcon />} 起動
                </Button>
                <Button variant="outline" disabled={!actions.load || busy !== null} onClick={() => act("load", "/strata/load")}>
                  {busy === "load" ? <Loader2Icon className="animate-spin" /> : <ZapIcon />} ロード
                </Button>
                <Button variant="outline" disabled={!actions.unload || busy !== null} onClick={() => act("unload", "/strata/unload")}>
                  {busy === "unload" ? <Loader2Icon className="animate-spin" /> : <PauseIcon />} アンロード
                </Button>
                <Button variant="outline" className="text-red-600 dark:text-red-400" disabled={!actions.stop || busy !== null} onClick={() => setStopOpen(true)}>
                  <SquareIcon /> 停止
                </Button>
              </div>
              <p className="text-xs text-muted-foreground">
                アンロードは VRAM を解放しますが、プロセスは起動したままです。次のリクエストで自動ロードされ、初回は時間がかかります。
              </p>
            </div>
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle>パフォーマンスモード</CardTitle>
          <CardDescription>モードによって Strata が使う CPU・GPU・VRAM の量が変わります</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-3">
          <div className="flex flex-wrap items-center gap-3">
            <ModeSwitch />
            {modeState && <span className="text-xs text-muted-foreground tabular-nums">{profileSummary(modeState)}</span>}
          </div>
          <p className="text-sm text-muted-foreground">
            {modeName === "shared"
              ? `共有モード: Strata はアイドル時のアンロードを早め、VRAM を一部予約し、CPU ワーカーを減らします${modeState ? ` (予約 ${gb(modeState.profile.vram_reserve_mb)} GB)` : ""}。`
              : "フル性能: Strata は GPU と CPU を制限せず使います。アイドル時のアンロードは通常の設定に従います。"}
          </p>
          <p className="text-xs text-muted-foreground">
            関連設定 (strata_lazy_load / strata_idle_unload_s / strata_shared_idle_unload_s / strata_watchdog / strata_swap_on_demand) は
            <a href="#admin" className="ml-1 underline underline-offset-2">
              管理ページ
            </a>
            で変更できます。
          </p>
        </CardContent>
      </Card>

      {installed && (
        <Card>
          <CardHeader>
            <CardTitle>メンテナンス</CardTitle>
            <CardDescription>ジョブはランタイムページと同じ仕組みで実行され、ログはこのページに表示されます。</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-3">
            <div className="flex flex-wrap gap-2">
              <Button variant={setupOpen || noConfigs ? "secondary" : "outline"} onClick={() => setSetupOpen((v) => !v)}>
                <WrenchIcon /> セットアップ案内
              </Button>
              <Button variant="outline" disabled={!update || jobRunning} title={update ? undefined : "ジョブが未登録です"} onClick={() => update && runJob(update)}>
                <RefreshCwIcon /> アップデート
              </Button>
              <Button variant="outline" disabled={!bench || jobRunning} title={bench ? undefined : "ジョブが未登録です"} onClick={() => bench && runJob(bench)}>
                <GaugeIcon /> ベンチマーク
              </Button>
            </div>
            {(setupOpen || noConfigs) && <SetupHint />}
            {jobLog && (
              <div className="grid min-w-0 gap-2">
                <div className="flex flex-wrap items-center gap-2 text-sm">
                  <ScrollTextIcon className="size-4 text-muted-foreground" />
                  <span className="min-w-0 truncate font-medium">{jobLog.name}</span>
                  {jobRun && <StatusBadge status={jobRun.status} />}
                  {jobRunning && (
                    <Button size="sm" variant="outline" className="ml-auto" onClick={stopJob}>
                      <SquareIcon /> 中断
                    </Button>
                  )}
                </div>
                <LogViewer path={`/jobs/runs/${jobLog.id}/logs`} className="h-64" />
              </div>
            )}
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle>接続先</CardTitle>
          <CardDescription>
            このマシンの API は次の URL で利用できます。認証には
            <a href="#security" className="mx-1 underline underline-offset-2">
              API キー
            </a>
            を使います。
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-2">
          {API_BASE.map((e) => {
            const url = `${origin}${e.path}`
            return (
              <div key={e.path} className="grid min-w-0 grid-cols-[1fr_auto] items-center gap-x-2 gap-y-1.5 rounded-lg border p-2 pl-3 sm:grid-cols-[10rem_minmax(0,1fr)_auto]">
                <span className="row-start-1 min-w-0 text-xs text-muted-foreground">{e.label}</span>
                <Button className="row-start-1 col-start-2 sm:col-start-3" size="icon-sm" variant="ghost" title="コピー" aria-label={`${e.label} の URL をコピー`} onClick={() => copyText(url)}>
                  <CopyIcon />
                </Button>
                <code className="col-span-2 row-start-2 min-w-0 font-mono text-sm break-all sm:col-span-1 sm:col-start-2 sm:row-start-1">{url}</code>
              </div>
            )
          })}
          <a href="#security" className="inline-flex w-fit items-center gap-1 text-xs text-muted-foreground underline underline-offset-2 hover:text-foreground">
            <ExternalLinkIcon className="size-3" /> API キーを発行・管理する
          </a>
        </CardContent>
      </Card>

      <ConfirmDialog
        open={stopOpen}
        onOpenChange={setStopOpen}
        title="Strata を停止しますか"
        description="プロセスを終了します。処理中のリクエストは中断され、再開するには起動し直す必要があります。"
        confirmLabel="停止"
        onConfirm={async () => {
          if (!status?.instance) return
          setBusy("stop")
          try {
            await api(`/instances/${status.instance.id}/stop`, { method: "POST" })
            toast.success("停止しました")
          } finally {
            setBusy(null)
            refetchStatus()
            refetchInstances()
          }
        }}
      />
    </div>
  )
}

/** Live "あと N 秒でアンロード"; ticks every second between the 2s status polls. */
function IdleCountdown({ status }: { status: StrataStatus }) {
  // Seconds since this status arrived: reset whenever a new poll result comes in.
  const [prev, setPrev] = useState(status)
  const [elapsed, setElapsed] = useState(0)
  if (prev !== status) {
    setPrev(status)
    setElapsed(0)
  }
  useEffect(() => {
    const id = setInterval(() => setElapsed((e) => e + 1), 1000)
    return () => clearInterval(id)
  }, [])

  const label = countdownLabel(idleCountdown(status, elapsed))
  if (!label) return null
  return (
    <div className="flex items-center gap-2 rounded-lg border border-sky-500/30 bg-sky-500/10 p-2 text-sm font-medium text-sky-700 tabular-nums dark:text-sky-300">
      <TimerIcon className="size-4 shrink-0" />
      {label}
    </div>
  )
}

/** Explains the ./setup.sh step and links to the runtimes page. */
function SetupHint() {
  return (
    <Alert>
      <WrenchIcon />
      <AlertTitle>Strata のセットアップ</AlertTitle>
      <AlertDescription className="grid gap-2">
        <span>リポジトリのルートで次のコマンドを実行すると、Strata が導入され設定が作られます。</span>
        <div className="flex min-w-0 items-center gap-2">
          <code className="min-w-0 rounded bg-muted px-2 py-1 font-mono text-xs">./setup.sh</code>
          <Button size="icon-sm" variant="ghost" title="コピー" aria-label="セットアップコマンドをコピー" onClick={() => copyText("./setup.sh")}>
            <CopyIcon />
          </Button>
        </div>
        <a href="#runtimes" className="w-fit text-xs underline underline-offset-2">
          ランタイムページで導入状況を確認 →
        </a>
      </AlertDescription>
    </Alert>
  )
}
