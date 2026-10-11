import { useEffect, useRef, useState } from "react"
import { ArmchairIcon, CheckCircle2Icon, ClipboardCopyIcon, ClockIcon, SaveIcon, TriangleAlertIcon, ZapIcon } from "lucide-react"
import { toast } from "sonner"

import { KV } from "@/components/stat"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import { Slider } from "@/components/ui/slider"
import { Switch } from "@/components/ui/switch"
import { usePoll, type ModeName, type ModeProfile, type ModeState, type Schedule, type Settings, type SharedLimits } from "@/lib/api"
import { MODE_LABEL, MODE_STYLE, SOURCE_LABEL, gb, useMode } from "@/lib/mode"
import { cn } from "@/lib/utils"

const BRIDGE_CMD = "powershell -ExecutionPolicy Bypass -File scripts\\windows\\setup-host.ps1"
const UBATCH = [64, 128, 256, 512, 1024]

const sliderVal = (v: number | readonly number[]) => (Array.isArray(v) ? (v[0] ?? 0) : (v as number))

function coresFor(physical: number, fraction: number) {
  return Math.min(Math.max(physical, 1), Math.max(1, Math.round(physical * Math.min(Math.max(fraction, 0.05), 1))))
}

function mibps(bps: number | null) {
  return bps == null ? "無制限" : `${(bps / 1024 / 1024).toFixed(0)} MiB/s`
}

/** Profile a mode would resolve to — the live one from the backend when it is the current mode. */
function previewProfile(state: ModeState, mode: ModeName, shared: SharedLimits, base?: Settings): ModeProfile {
  if (state.mode === mode && (mode === "full" || JSON.stringify(state.settings.shared) === JSON.stringify(shared))) return state.profile
  const phys = state.topology.physical_cores
  const ratio = state.topology.logical_cores / Math.max(phys, 1)
  if (mode === "full") {
    return {
      mode,
      cpu_set: [],
      physical_cores: phys,
      logical_cpus: state.topology.logical_cores,
      vram_reserve_mb: 512,
      gpu_power_percent: 100,
      ubatch_cap: 2048,
      nice: 0,
      max_concurrent_downloads: base?.max_concurrent_downloads ?? 0,
      connections_per_file: base?.connections_per_file ?? 0,
      download_bytes_per_sec: null,
      vmmem_priority: "Normal",
    }
  }
  const n = coresFor(phys, shared.cpu_fraction)
  return {
    mode,
    cpu_set: [],
    physical_cores: n,
    logical_cpus: Math.round(n * ratio),
    vram_reserve_mb: shared.vram_reserve_mb,
    gpu_power_percent: shared.gpu_power_percent,
    ubatch_cap: shared.ubatch_cap,
    nice: shared.nice,
    max_concurrent_downloads: shared.max_concurrent_downloads,
    connections_per_file: shared.connections_per_file,
    download_bytes_per_sec: shared.download_mib_per_sec ? shared.download_mib_per_sec * 1024 * 1024 : null,
    vmmem_priority: shared.vmmem_below_normal ? "BelowNormal" : "Normal",
  }
}

function ProfileCard({ mode, p, physical, current, note }: { mode: ModeName; p: ModeProfile; physical: number; current: boolean; note: string }) {
  const Icon = mode === "full" ? ZapIcon : ArmchairIcon
  const st = MODE_STYLE[mode]
  return (
    <Card className={cn(current && ["ring-2", mode === "full" ? "ring-emerald-500/50" : "ring-amber-500/50"])}>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Icon className={cn("size-4", st.text)} /> {MODE_LABEL[mode]}
          {current && <Badge className={cn("ml-auto", st.active)}>現在</Badge>}
        </CardTitle>
        <CardDescription>{note}</CardDescription>
      </CardHeader>
      <CardContent className="grid grid-cols-2 gap-3 sm:grid-cols-3">
        <KV k="使用コア" v={`${p.physical_cores} / ${physical}`} />
        <KV k="スレッド (-t / -tb)" v={`${p.physical_cores} / ${p.logical_cpus}`} />
        <KV k="VRAM 予約" v={`${gb(p.vram_reserve_mb)} GB`} />
        <KV k="GPU 電力" v={`${p.gpu_power_percent}%`} />
        <KV k="micro-batch 上限" v={p.ubatch_cap} />
        <KV k="nice" v={p.nice} />
        <KV k="同時 DL 数" v={p.max_concurrent_downloads || "—"} />
        <KV k="DL 接続数/ファイル" v={p.connections_per_file || "—"} />
        <KV k="DL 帯域" v={mibps(p.download_bytes_per_sec)} />
      </CardContent>
    </Card>
  )
}

function Field({ label, value, hint, children }: { label: string; value?: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="grid gap-2">
      <div className="flex items-baseline justify-between gap-2">
        <Label>{label}</Label>
        {value && <span className="text-sm font-medium tabular-nums">{value}</span>}
      </div>
      {children}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  )
}

function fmtTime(ts: number | null | undefined) {
  return ts ? new Date(ts * 1000).toLocaleString("ja-JP", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "—"
}

function bridgeSummary(r: unknown): string | null {
  if (r == null) return null
  if (typeof r === "string") return r
  try {
    return JSON.stringify(r)
  } catch {
    return null
  }
}

export function ModePage() {
  const { state, saveSettings } = useMode()
  const { data: base } = usePoll<Settings>("/settings", 0)
  const [shared, setShared] = useState<SharedLimits | null>(null)
  const [schedule, setSchedule] = useState<Schedule | null>(null)
  const [dlCap, setDlCap] = useState("")
  const init = useRef(false)

  // Seed the editors once; after that the user's edits are not clobbered by polling.
  useEffect(() => {
    if (!state || init.current) return
    init.current = true
    setShared(state.settings.shared)
    setSchedule(state.settings.schedule)
    setDlCap(state.settings.shared.download_mib_per_sec ? String(state.settings.shared.download_mib_per_sec) : "")
  }, [state])

  if (!state || !shared || !schedule) {
    return (
      <div className="grid gap-4">
        <Skeleton className="h-40" />
        <Skeleton className="h-64" />
      </div>
    )
  }

  const phys = state.topology.physical_cores
  const upd = (patch: Partial<SharedLimits>) => setShared((s) => (s ? { ...s, ...patch } : s))
  const sched = (patch: Partial<Schedule>) => setSchedule((s) => (s ? { ...s, ...patch } : s))
  const cap = dlCap.trim() === "" ? null : Math.max(0, Number(dlCap) || 0) || null
  const sharedToSave: SharedLimits = { ...shared, download_mib_per_sec: cap }
  const bridgeNote = bridgeSummary(state.bridge.last_result)

  return (
    <div className="grid gap-4">
      <div className="grid gap-4 xl:grid-cols-2">
        <ProfileCard
          mode="full"
          p={previewProfile(state, "full", state.settings.shared, base)}
          physical={phys}
          current={state.mode === "full"}
          note="全コア・全 VRAM・GPU 電力は既定値で、ダウンロードも無制限。"
        />
        <ProfileCard
          mode="shared"
          p={previewProfile(state, "shared", sharedToSave, base)}
          physical={phys}
          current={state.mode === "shared"}
          note="人が使えるよう CPU/GPU を絞る。下の編集内容は保存前でもここに反映されます。"
        />
      </div>

      <p className="text-xs text-muted-foreground">
        現在: <span className={cn("font-medium", MODE_STYLE[state.mode].text)}>{MODE_LABEL[state.mode]}</span> ({SOURCE_LABEL[state.source] ?? state.source}
        {state.switched_at ? ` · ${fmtTime(state.switched_at)} に切替` : ""})
      </p>

      <Card>
        <CardHeader>
          <CardTitle>共有モードの制限</CardTitle>
          <CardDescription>
            推論プロセスの CPU はコア単位で固定 (上位のコアを使用し、Windows が使う下位コアを空ける)。CPU の割当と優先度は稼働中のサーバにも即時反映され、スレッド数・バッチサイズは自動チューニングのサーバの再起動で反映されます。
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-6 md:grid-cols-2">
          <Field
            label="CPU 割当"
            value={`${Math.round(shared.cpu_fraction * 100)}% = ${coresFor(phys, shared.cpu_fraction)} / ${phys} コア`}
            hint="物理コア数に対する割合 (10–100%)"
          >
            <Slider min={10} max={100} step={5} value={[Math.round(shared.cpu_fraction * 100)]} onValueChange={(v) => upd({ cpu_fraction: sliderVal(v) / 100 })} />
          </Field>
          <Field label="デスクトップ用 VRAM 予約" value={`${gb(shared.vram_reserve_mb)} GB`} hint="モデルと KV キャッシュはこの分を除いた空きに収まるよう計算されます (0–16 GB)">
            <Slider min={0} max={16} step={0.5} value={[shared.vram_reserve_mb / 1024]} onValueChange={(v) => upd({ vram_reserve_mb: Math.round(sliderVal(v) * 1024) })} />
          </Field>
          <Field label="GPU 電力制限" value={`${shared.gpu_power_percent}%`} hint="ボード既定の電力制限に対する割合 (30–100%)。Windows ブリッジが必要です">
            <Slider min={30} max={100} step={5} value={[shared.gpu_power_percent]} onValueChange={(v) => upd({ gpu_power_percent: sliderVal(v) })} />
          </Field>
          <Field label="nice 値" value={String(shared.nice)} hint="大きいほど他のアプリを優先 (0–19)">
            <Slider min={0} max={19} step={1} value={[shared.nice]} onValueChange={(v) => upd({ nice: sliderVal(v) })} />
          </Field>
          <Field label="micro-batch 上限 (-ub)" hint="小さいほどデスクトップに GPU の時間を譲りやすい (プロンプト処理は遅くなる)">
            <Select value={String(shared.ubatch_cap)} onValueChange={(v) => upd({ ubatch_cap: Number(v) })}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {(UBATCH.includes(shared.ubatch_cap) ? UBATCH : [...UBATCH, shared.ubatch_cap].sort((a, b) => a - b)).map((n) => (
                  <SelectItem key={n} value={String(n)}>
                    {n}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
          <div className="grid gap-2">
            <Label>ダウンロード制限</Label>
            <div className="grid grid-cols-3 gap-2">
              <div className="grid gap-1">
                <span className="text-xs text-muted-foreground">同時 DL 数</span>
                <Input
                  type="number"
                  min={1}
                  value={shared.max_concurrent_downloads}
                  onChange={(e) => upd({ max_concurrent_downloads: Math.max(1, Number(e.target.value) || 1) })}
                />
              </div>
              <div className="grid gap-1">
                <span className="text-xs text-muted-foreground">接続数/ファイル</span>
                <Input
                  type="number"
                  min={1}
                  value={shared.connections_per_file}
                  onChange={(e) => upd({ connections_per_file: Math.max(1, Number(e.target.value) || 1) })}
                />
              </div>
              <div className="grid gap-1">
                <span className="text-xs text-muted-foreground">上限 MiB/s</span>
                <Input type="number" min={0} placeholder="無制限" value={dlCap} onChange={(e) => setDlCap(e.target.value)} />
              </div>
            </div>
            <p className="text-xs text-muted-foreground">帯域上限を空欄にすると無制限</p>
          </div>
          <div className="flex items-start gap-3 md:col-span-2">
            <Switch id="vmmem" checked={shared.vmmem_below_normal} onCheckedChange={(v) => upd({ vmmem_below_normal: v })} />
            <div className="grid gap-0.5">
              <Label htmlFor="vmmem" className="font-normal">
                WSL VM (vmmem) の優先度を「通常以下」にする
              </Label>
              <span className="text-xs text-muted-foreground">Windows 側のフォアグラウンドアプリを優先します (ブリッジ経由)</span>
            </div>
          </div>
        </CardContent>
        <CardFooter>
          <Button onClick={() => saveSettings({ shared: sharedToSave })}>
            <SaveIcon /> 保存
          </Button>
        </CardFooter>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>切替時の動作</CardTitle>
            <CardDescription>CPU アフィニティと優先度は常に稼働中のサーバへ即時適用されます。</CardDescription>
          </CardHeader>
          <CardContent>
            <div className="flex items-start gap-3">
              <Switch
                id="auto_restart"
                checked={state.settings.auto_restart}
                onCheckedChange={(v) => saveSettings({ auto_restart: v })}
              />
              <div className="grid gap-0.5">
                <Label htmlFor="auto_restart" className="font-normal">
                  モード切替時に自動チューニングのサーバを再起動
                </Label>
                <span className="text-xs text-muted-foreground">スレッド数・micro-batch・VRAM 見積りを新しいモードに合わせ直します。再起動中は応答が途切れます。</span>
              </div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Windows ブリッジ</CardTitle>
            <CardDescription>GPU 電力制限と vmmem 優先度は Windows ホスト側の操作が必要です。</CardDescription>
          </CardHeader>
          <CardContent>
            {state.bridge.available ? (
              <Alert>
                <CheckCircle2Icon className="text-emerald-500" />
                <AlertTitle>GPU 電力制限・vmmem 優先度も切替</AlertTitle>
                <AlertDescription>
                  {state.bridge.message}
                  {bridgeNote && <span className="mt-1 block break-all font-mono text-[11px]">直近の結果: {bridgeNote}</span>}
                </AlertDescription>
              </Alert>
            ) : (
              <Alert>
                <TriangleAlertIcon className="text-amber-500" />
                <AlertTitle>Windows ブリッジが利用できません</AlertTitle>
                <AlertDescription>
                  <p>{state.bridge.message}</p>
                  <p className="mt-2">Windows の PowerShell で次を実行してセットアップしてください:</p>
                  <div className="mt-1 flex items-center gap-2 rounded bg-muted px-2 py-1">
                    <code className="min-w-0 flex-1 break-all font-mono text-[11px]">{BRIDGE_CMD}</code>
                    <Button
                      size="icon-sm"
                      variant="ghost"
                      title="コピー"
                      onClick={() => {
                        navigator.clipboard.writeText(BRIDGE_CMD)
                        toast.success("コピーしました")
                      }}
                    >
                      <ClipboardCopyIcon />
                    </Button>
                  </div>
                  <p className="mt-2 text-xs">未設定でも CPU 制限・VRAM 予約・バッチ制限・ダウンロード制限は有効です。</p>
                </AlertDescription>
              </Alert>
            )}
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <ClockIcon className="size-4" /> スケジュール
          </CardTitle>
          <CardDescription>時間帯で自動的にモードを切り替えます (WSL のローカル時刻)。窓の外は共有モードになります。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          <div className="flex items-center gap-3">
            <Switch id="sch_enabled" checked={schedule.enabled} onCheckedChange={(v) => sched({ enabled: v })} />
            <Label htmlFor="sch_enabled" className="font-normal">
              スケジュール切替を有効にする
            </Label>
            {schedule.enabled && state.scheduled_mode && (
              <Badge variant="outline" className={cn("ml-auto", MODE_STYLE[state.scheduled_mode].text)}>
                現在の予定: {MODE_LABEL[state.scheduled_mode]}
              </Badge>
            )}
          </div>
          <div className="grid max-w-md grid-cols-2 gap-3">
            <div className="grid gap-1.5">
              <Label htmlFor="full_start">フル性能 開始</Label>
              <Input id="full_start" type="time" value={schedule.full_start} onChange={(e) => sched({ full_start: e.target.value })} />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="full_end">フル性能 終了</Label>
              <Input id="full_end" type="time" value={schedule.full_end} onChange={(e) => sched({ full_end: e.target.value })} />
            </div>
          </div>
          <p className="-mt-2 text-xs text-muted-foreground">終了が開始より早い場合は日をまたぎます (例 20:00 → 08:00)。</p>
          <div className="flex items-center gap-3">
            <Switch id="weekends" checked={schedule.weekends_full} onCheckedChange={(v) => sched({ weekends_full: v })} />
            <Label htmlFor="weekends" className="font-normal">
              土日は終日フル性能
            </Label>
          </div>
          {schedule.enabled && state.next_switch_at ? (
            <p className="text-sm text-muted-foreground">
              次の切替: <span className="font-medium text-foreground">{fmtTime(state.next_switch_at)}</span>
            </p>
          ) : null}
        </CardContent>
        <CardFooter>
          <Button onClick={() => saveSettings({ schedule })}>
            <SaveIcon /> 保存
          </Button>
        </CardFooter>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>各モードの内容</CardTitle>
        </CardHeader>
        <CardContent className="grid gap-4 text-sm md:grid-cols-2">
          <div className="space-y-1">
            <div className={cn("flex items-center gap-1.5 font-medium", MODE_STYLE.full.text)}>
              <ZapIcon className="size-4" /> フル性能
            </div>
            <p className="text-muted-foreground">
              ワークステーションを推論専用に使います。全コア・全 VRAM を使い、GPU 電力制限は既定のまま、ダウンロードも帯域無制限で動きます。
            </p>
          </div>
          <div className="space-y-1">
            <div className={cn("flex items-center gap-1.5 font-medium", MODE_STYLE.shared.text)}>
              <ArmchairIcon className="size-4" /> 共有モード
            </div>
            <p className="text-muted-foreground">
              人が同じマシンを使えるよう、推論を一部のコアに固定して nice を上げ、デスクトップ用に VRAM を確保し、GPU 電力・micro-batch・ダウンロード帯域を制限します。
            </p>
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
