import { AlertTriangleIcon, CheckCircle2Icon, CpuIcon, HardDriveIcon, InfoIcon, MemoryStickIcon, XCircleIcon, ZapIcon } from "lucide-react"

import { MODE_ICON } from "@/components/mode-switch"
import { KV, Meter } from "@/components/stat"
import { StatusBadge } from "@/components/status-badge"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePoll, type Check, type Download, type Instance, type SystemSnapshot } from "@/lib/api"
import { bytes, duration, pct } from "@/lib/format"
import { MODE_LABEL, MODE_STYLE, SOURCE_LABEL, gb, useMode } from "@/lib/mode"
import { cn } from "@/lib/utils"

export function DashboardPage() {
  const { data: sys } = usePoll<SystemSnapshot>("/system", 1500)
  const { data: checks } = usePoll<Check[]>("/system/checks", 30000)
  const { data: inst } = usePoll<Instance[]>("/instances", 2000)
  const { data: dls } = usePoll<Download[]>("/downloads", 2000)

  const { state: mode } = useMode()
  const running = (inst ?? []).filter((i) => i.status === "running" || i.status === "starting")
  const active = (dls ?? []).filter((d) => d.status === "downloading" || d.status === "verifying" || d.status === "queued")

  return (
    <div className="grid gap-4">
      {mode && (
        <a
          href="#mode"
          className={cn("flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl border p-3 transition-colors hover:brightness-95", MODE_STYLE[mode.mode].border, MODE_STYLE[mode.mode].bg)}
        >
          <div className="flex items-center gap-2">
            {mode.mode === "full" ? <MODE_ICON.full className={cn("size-5", MODE_STYLE.full.text)} /> : <MODE_ICON.shared className={cn("size-5", MODE_STYLE.shared.text)} />}
            <div className="leading-tight">
              <div className={cn("font-medium", MODE_STYLE[mode.mode].text)}>{MODE_LABEL[mode.mode]}</div>
              <div className="text-[11px] text-muted-foreground">{SOURCE_LABEL[mode.source] ?? mode.source}</div>
            </div>
          </div>
          <KV k="CPU" v={`${mode.profile.physical_cores} / ${mode.topology.physical_cores} コア`} />
          <KV k="VRAM 予約" v={`${gb(mode.profile.vram_reserve_mb)} GB`} />
          <KV k="GPU 電力" v={`${mode.profile.gpu_power_percent}%`} />
          <KV k="micro-batch 上限" v={mode.profile.ubatch_cap} />
          {(inst ?? []).some((i) => i.stale && (i.status === "running" || i.status === "starting")) && (
            <Badge className="bg-amber-500/15 text-amber-700 dark:text-amber-400">モード不一致のサーバあり</Badge>
          )}
          <span className="ml-auto text-xs text-muted-foreground">詳細・設定 →</span>
        </a>
      )}
      <div className="grid gap-4 lg:grid-cols-2">
        {sys ? (
          sys.gpus.length ? (
            sys.gpus.map((g) => (
              <Card key={g.index}>
                <CardHeader>
                  <CardTitle className="flex items-center gap-2">
                    <ZapIcon className="size-4 text-emerald-500" /> {g.name}
                  </CardTitle>
                  <CardDescription>
                    GPU {g.index} · Driver {g.driver} · PCIe Gen{g.pcie_gen} x{g.pcie_width}
                  </CardDescription>
                </CardHeader>
                <CardContent className="grid gap-3">
                  <Meter label="VRAM" value={pct(g.memory_used_mb, g.memory_total_mb)} detail={`${bytes(g.memory_used_mb * 2 ** 20)} / ${bytes(g.memory_total_mb * 2 ** 20)}`} />
                  <Meter label="GPU 使用率" value={g.util_gpu} tone={undefined} />
                  <div className="grid grid-cols-4 gap-2 pt-1">
                    <KV k="温度" v={`${g.temperature}°C`} />
                    <KV k="電力" v={g.power_draw != null ? `${g.power_draw.toFixed(0)} / ${g.power_limit?.toFixed(0)} W` : "—"} />
                    <KV k="SM クロック" v={`${g.clock_sm} MHz`} />
                    <KV k="メモリ帯域使用" v={`${g.util_mem}%`} />
                  </div>
                </CardContent>
              </Card>
            ))
          ) : (
            <Card>
              <CardHeader>
                <CardTitle>GPU 未検出</CardTitle>
                <CardDescription>nvidia-smi が見つかりません。Windows 側に NVIDIA ドライバを入れ、WSL を再起動してください。</CardDescription>
              </CardHeader>
            </Card>
          )
        ) : (
          <Skeleton className="h-48" />
        )}

        {sys ? (
          <Card>
            <CardHeader>
              <CardTitle className="flex items-center gap-2">
                <CpuIcon className="size-4 text-sky-500" /> {sys.cpu.model}
              </CardTitle>
              <CardDescription>
                {sys.cpu.physical_cores} コア / {sys.cpu.logical_cores} スレッド · load {sys.cpu.load_avg.map((l) => l.toFixed(1)).join(" ")}
              </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-3">
              <Meter label="CPU 全体" value={sys.cpu.percent} />
              <div className="grid grid-cols-[repeat(auto-fill,minmax(14px,1fr))] gap-1" title="コア別使用率">
                {sys.cpu.per_core.map((c, i) => (
                  <div key={i} className="h-4 rounded-sm bg-sky-500" style={{ opacity: 0.12 + (c / 100) * 0.88 }} title={`cpu${i}: ${c}%`} />
                ))}
              </div>
              <div className="grid grid-cols-2 gap-3 pt-1">
                <div className="space-y-1">
                  <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                    <MemoryStickIcon className="size-3.5" /> WSL メモリ
                  </div>
                  <Meter label="" value={sys.memory.percent} detail={`${bytes(sys.memory.used)} / ${bytes(sys.memory.total)}`} />
                </div>
                <div className="space-y-1">
                  <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                    <HardDriveIcon className="size-3.5" /> モデル置き場
                  </div>
                  <Meter label="" value={pct(sys.disk.used, sys.disk.total)} detail={`空き ${bytes(sys.disk.free)}`} />
                </div>
              </div>
            </CardContent>
          </Card>
        ) : (
          <Skeleton className="h-48" />
        )}
      </div>

      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>稼働中のサーバ</CardTitle>
            <CardDescription>トークン速度は llama-server / vLLM の /metrics から取得</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-2">
            {running.length === 0 && <p className="text-sm text-muted-foreground">起動中のサーバはありません</p>}
            {running.map((i) => (
              <div key={i.id} className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-lg border p-3">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate font-medium">{i.name}</span>
                    <StatusBadge status={i.status} />
                  </div>
                  <div className="font-mono text-xs text-muted-foreground">
                    {i.runtime} · http://localhost:{i.port}/v1
                  </div>
                </div>
                <KV k="生成 tok/s" v={i.metrics.gen_tps?.toFixed(1) ?? "—"} />
                <KV k="プロンプト tok/s" v={i.metrics.prompt_tps?.toFixed(0) ?? "—"} />
                <KV k="処理中" v={i.metrics.processing ?? "—"} />
                <KV k="KV 使用" v={i.metrics.kv_usage != null ? `${(i.metrics.kv_usage * 100).toFixed(0)}%` : "—"} />
              </div>
            ))}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>ダウンロード</CardTitle>
            <CardDescription>{active.length} 件進行中</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-3">
            {active.length === 0 && <p className="text-sm text-muted-foreground">進行中のダウンロードはありません</p>}
            {active.slice(0, 5).map((d) => (
              <Meter
                key={d.id}
                label={d.repo}
                value={pct(d.downloaded, d.total)}
                detail={`${bytes(d.speed)}/s · ${duration(d.eta)}`}
                tone={undefined}
              />
            ))}
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>環境チェック</CardTitle>
          <CardDescription>WSL2 で性能を落としがちなポイント</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-2 md:grid-cols-2">
          {(checks ?? []).map((c) => {
            const Icon = c.ok ? (c.level === "info" ? InfoIcon : CheckCircle2Icon) : c.level === "error" ? XCircleIcon : AlertTriangleIcon
            return (
              <div key={c.id} className="flex items-start gap-2 text-sm">
                <Icon
                  className={cn(
                    "mt-0.5 size-4 shrink-0",
                    c.ok ? "text-emerald-500" : c.level === "error" ? "text-red-500" : c.level === "warn" ? "text-amber-500" : "text-muted-foreground",
                  )}
                />
                <span>{c.message}</span>
                {!c.ok && c.level !== "info" && (
                  <Badge variant="outline" className="ml-auto">
                    要対応
                  </Badge>
                )}
              </div>
            )
          })}
        </CardContent>
      </Card>
    </div>
  )
}
