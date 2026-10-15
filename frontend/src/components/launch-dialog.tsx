import { useEffect, useMemo, useState } from "react"
import { RocketIcon, SaveIcon, SparklesIcon } from "lucide-react"
import { toast } from "sonner"

import { Meter } from "@/components/stat"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"
import { api, usePoll, type LocalModel, type Profile, type TuneResult } from "@/lib/api"
import { bytes, joinArgs, pct, splitArgs } from "@/lib/format"
import { MODE_LABEL, useMode } from "@/lib/mode"

const RUNTIMES = [
  { id: "strata", label: "Strata (メインエンジン・既定)" },
  { id: "llama.cpp", label: "llama.cpp (補助)" },
  { id: "vllm", label: "vLLM (補助)" },
  { id: "ollama", label: "Ollama (補助)" },
]

export function LaunchDialog({
  open,
  onOpenChange,
  initial,
  onLaunched,
}: {
  open: boolean
  onOpenChange: (o: boolean) => void
  initial?: Partial<Profile>
  onLaunched?: () => void
}) {
  const { data: models } = usePoll<LocalModel[]>(open ? "/models" : null, 0)
  const [runtime, setRuntime] = useState("strata")
  const [model, setModel] = useState("")
  const [name, setName] = useState("")
  const [port, setPort] = useState("")
  const [args, setArgs] = useState("")
  const [ctx, setCtx] = useState("")
  const [kv, setKv] = useState("q8_0")
  const [parallel, setParallel] = useState("1")
  const [tune, setTune] = useState<TuneResult | null>(null)
  const [autostart, setAutostart] = useState(false)
  const [autoTuneOn, setAutoTuneOn] = useState(true)
  const { state: modeState } = useMode()
  const modeName = modeState?.mode
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    if (!open) return
    setRuntime(initial?.runtime ?? "strata")
    setModel(initial?.model ?? "")
    setName(initial?.name ?? "")
    setPort(initial?.port ? String(initial.port) : "")
    setArgs(joinArgs(initial?.args ?? []))
    setAutostart(initial?.autostart ?? false)
    setAutoTuneOn(initial?.auto_tune ?? true)
    setTune(null)
  }, [open, initial])

  const candidates = useMemo(() => (models ?? []).filter((m) => m.runtimes.includes(runtime) && runtime !== "ollama"), [models, runtime])
  const selected = candidates.find((m) => m.id === model)

  // true when the backend computes the tuned args itself and `args` are only user overrides
  const autoManaged = autoTuneOn && runtime !== "ollama" && runtime !== "strata"

  async function autoTune() {
    if (!model) return toast.error("モデルを選択してください")
    try {
      const r = await api<TuneResult>("/tune", {
        method: "POST",
        json: { model, ctx: ctx ? Number(ctx) : null, kv_type: kv, parallel: Number(parallel) || 1 },
      })
      setTune(r)
      if (!autoManaged) {
        setArgs(joinArgs(r.args))
        if (r.estimate.ctx) setCtx(String(r.estimate.ctx))
      }
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  // Auto-tune as soon as a GGUF model is picked so the defaults are already sensible.
  useEffect(() => {
    if (open && runtime === "llama.cpp" && model && (autoManaged || !initial?.args?.length)) autoTune()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, runtime, open, modeName, autoManaged])

  function toggleAutoTune(on: boolean) {
    setAutoTuneOn(on)
    if (on) setArgs("")
    else if (tune) setArgs(joinArgs(tune.args))
  }

  /** In auto mode the dialog's own fields become explicit overrides on top of the tuned args. */
  function effectiveArgs(): string[] {
    const user = splitArgs(args)
    if (!autoManaged || runtime !== "llama.cpp") return user
    const has = (...flags: string[]) => user.some((a) => flags.includes(a))
    const extra: string[] = []
    if (ctx && !has("-c", "--ctx-size")) extra.push("-c", ctx)
    if (!has("-ctk", "--cache-type-k")) extra.push("-ctk", kv)
    if (!has("-ctv", "--cache-type-v")) extra.push("-ctv", kv)
    if (Number(parallel) > 0 && !has("-np", "--parallel")) extra.push("-np", String(Number(parallel)))
    return [...user, ...extra]
  }

  const spec = (): Profile => ({
    ...(initial?.id ? { id: initial.id } : {}),
    name,
    runtime,
    model: runtime === "ollama" ? "" : model,
    port: port ? Number(port) : null,
    args: effectiveArgs(),
    env: initial?.env ?? {},
    autostart,
    auto_tune: autoManaged,
  })

  async function launch() {
    setBusy(true)
    try {
      await api("/instances", { method: "POST", json: spec() })
      toast.success("起動しました。ロード完了まで数十秒かかることがあります")
      onLaunched?.()
      onOpenChange(false)
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  async function save() {
    try {
      await api("/profiles", { method: "POST", json: { ...spec(), name: name || selected?.name || runtime } })
      toast.success("プロファイルを保存しました")
      onLaunched?.()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  const est = tune?.estimate
  const need = (est?.model_bytes ?? 0) + (est?.kv_bytes ?? 0)

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>サーバを起動</DialogTitle>
          <DialogDescription>OpenAI 互換 API (/v1) として公開されます。通常は Strata をお使いください。llama.cpp / vLLM / Ollama は補助ランタイムです。</DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          <div className="grid grid-cols-2 gap-3">
            <div className="grid gap-1.5">
              <Label>ランタイム</Label>
              <Select value={runtime} onValueChange={(v) => setRuntime(String(v))}>
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {RUNTIMES.map((r) => (
                    <SelectItem key={r.id} value={r.id}>
                      {r.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-1.5">
              <Label>名前 / ポート</Label>
              <div className="flex gap-2">
                <Input placeholder="(自動)" value={name} onChange={(e) => setName(e.target.value)} />
                <Input className="w-24" placeholder="auto" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
              </div>
            </div>
          </div>

          {runtime !== "ollama" && (
            <div className="grid gap-1.5">
              <Label>モデル</Label>
              <Select value={model} onValueChange={(v) => setModel(String(v))}>
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="ローカルモデルを選択" />
                </SelectTrigger>
                <SelectContent>
                  {candidates.map((m) => (
                    <SelectItem key={m.id} value={m.id}>
                      <span className="truncate">{m.id}</span>
                      <span className="ml-auto pl-2 text-xs text-muted-foreground">{bytes(m.size)}</span>
                    </SelectItem>
                  ))}
                  {candidates.length === 0 && <div className="p-2 text-sm text-muted-foreground">該当モデルなし (ダウンロードから取得)</div>}
                </SelectContent>
              </Select>
            </div>
          )}

          {runtime === "llama.cpp" && (
            <div className="grid grid-cols-[1fr_1fr_1fr_auto] items-end gap-3">
              <div className="grid gap-1.5">
                <Label>Context</Label>
                <Input value={ctx} placeholder="auto" onChange={(e) => setCtx(e.target.value.replace(/\D/g, ""))} />
              </div>
              <div className="grid gap-1.5">
                <Label>KV cache</Label>
                <Select value={kv} onValueChange={(v) => setKv(String(v))}>
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="f16">f16 (最高品質)</SelectItem>
                    <SelectItem value="q8_0">q8_0 (推奨)</SelectItem>
                    <SelectItem value="q4_0">q4_0 (省メモリ)</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div className="grid gap-1.5">
                <Label>並列スロット</Label>
                <Input value={parallel} onChange={(e) => setParallel(e.target.value.replace(/\D/g, ""))} />
              </div>
              <Button variant="secondary" onClick={autoTune}>
                <SparklesIcon /> 自動チューニング
              </Button>
            </div>
          )}

          {tune && est?.vram_total ? (
            <div className="space-y-2 rounded-lg border p-3">
              <Meter
                label={`VRAM 見積り (モデル ${bytes(est.model_bytes)} + KV ${bytes(est.kv_bytes)}${est.vram_reserve ? ` · デスクトップ用予約 ${bytes(est.vram_reserve)}` : ""})`}
                value={pct(need, est.vram_free ?? 0)}
                detail={`${bytes(need)} / 空き ${bytes(est.vram_free)}`}
                tone={est.fits ? undefined : "danger"}
              />
              <ul className="list-disc space-y-0.5 pl-5 text-xs text-muted-foreground">
                {tune.notes.map((n) => (
                  <li key={n}>{n}</li>
                ))}
              </ul>
            </div>
          ) : tune ? (
            <Alert>
              <AlertDescription className="text-xs">{tune.notes.join(" ")}</AlertDescription>
            </Alert>
          ) : null}

          {runtime !== "ollama" && runtime !== "strata" && (
            <div className="flex items-start gap-2">
              <Switch checked={autoTuneOn} onCheckedChange={toggleAutoTune} id="auto_tune" className="mt-0.5" />
              <div className="grid gap-0.5">
                <Label htmlFor="auto_tune" className="font-normal">
                  自動チューニング (モードに追従)
                </Label>
                <span className="text-xs text-muted-foreground">
                  現在のモード{modeName ? ` (${MODE_LABEL[modeName]})` : ""}の制限に合わせて引数を計算し、モード切替時の再起動でも再計算します。
                </span>
              </div>
            </div>
          )}

          {autoManaged && tune && tune.args.length > 0 && (
            <div className="grid gap-1.5">
              <Label className="text-muted-foreground">チューニング結果 (読み取り専用)</Label>
              <code className="block max-h-28 overflow-y-auto rounded bg-muted px-2 py-1.5 font-mono text-[11px] break-all whitespace-pre-wrap text-muted-foreground">
                {joinArgs(tune.args)}
              </code>
            </div>
          )}

          <div className="grid gap-1.5">
            <Label>{autoManaged ? "上書き引数 (任意)" : "追加引数"}</Label>
            <Textarea
              className="min-h-20 font-mono text-xs"
              value={args}
              placeholder={autoManaged ? "例: --n-gpu-layers 20" : undefined}
              onChange={(e) => setArgs(e.target.value)}
            />
          </div>

          <div className="flex items-center gap-2">
            <Switch checked={autostart} onCheckedChange={setAutostart} id="autostart" />
            <Label htmlFor="autostart" className="font-normal">ハーネス起動時に自動起動 (プロファイル保存時)</Label>
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={save}>
            <SaveIcon /> プロファイル保存
          </Button>
          <Button onClick={launch} disabled={busy}>
            <RocketIcon /> 起動
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
