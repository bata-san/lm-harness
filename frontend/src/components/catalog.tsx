import { useEffect, useState } from "react"
import { CheckCircle2Icon, DownloadIcon, HeartIcon, Loader2Icon, RefreshCwIcon, SearchIcon, TriangleAlertIcon } from "lucide-react"
import { toast } from "sonner"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { api, usePoll, type CatalogItem, type CatalogPlan, type Fit, type SystemSnapshot } from "@/lib/api"
import { ago, bytes, compact } from "@/lib/format"
import { cn } from "@/lib/utils"

type Sort = "trending" | "recent" | "downloads"
const SORT_LABEL: Record<Sort, string> = { trending: "トレンド", recent: "新着", downloads: "人気" }

export const FIT_META: Record<Fit, { label: string; cls: string; explain: string }> = {
  gpu: { label: "全層GPU", cls: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400", explain: "すべての層が VRAM に載るため最速で動作します。" },
  moe_offload: { label: "MoEをCPUへ一部退避", cls: "bg-sky-500/15 text-sky-600 dark:text-sky-400", explain: "MoE のエキスパート層の一部を CPU メモリへ退避して動かします。実用的な速度が出ます。" },
  partial: { label: "一部CPU", cls: "bg-amber-500/15 text-amber-700 dark:text-amber-400", explain: "一部の層を CPU で処理します。GPU のみより遅くなります。" },
  cpu: { label: "CPUのみ", cls: "bg-red-500/15 text-red-600 dark:text-red-400", explain: "GPU にほとんど載らず CPU 推論になります。かなり遅くなります。" },
  too_big: { label: "容量超過", cls: "bg-red-500/15 text-red-600 dark:text-red-400", explain: "このマシンのメモリ (VRAM + RAM) に収まらないため動作しません。" },
}

export function FitBadge({ fit, className }: { fit: Fit; className?: string }) {
  const m = FIT_META[fit]
  return (
    <Badge variant="secondary" className={cn("border-0", m.cls, className)} title={m.explain}>
      {m.label}
    </Badge>
  )
}

/** Free bytes on the models disk, from /api/system. */
function useDiskFree(): number | null {
  const { data } = usePoll<SystemSnapshot>("/system", 10000)
  return data?.disk?.free ?? null
}

const encRepo = (r: string) => r.split("/").map(encodeURIComponent).join("/")

export function CatalogPanel({ onQueued }: { onQueued: () => void }) {
  const [sort, setSort] = useState<Sort>("trending")
  const [input, setInput] = useState("")
  const [q, setQ] = useState("")
  const [items, setItems] = useState<CatalogItem[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [reload, setReload] = useState(0)
  const [planRepo, setPlanRepo] = useState<string | null>(null)

  // Debounce the search box.
  useEffect(() => {
    const id = setTimeout(() => setQ(input.trim()), 400)
    return () => clearTimeout(id)
  }, [input])

  useEffect(() => {
    let alive = true
    setLoading(true)
    setError(null)
    api<CatalogItem[]>(`/catalog?sort=${sort}&q=${encodeURIComponent(q)}&gguf=true`)
      .then((r) => alive && setItems(r))
      .catch((e) => alive && setError((e as Error).message))
      .finally(() => alive && setLoading(false))
    return () => {
      alive = false
    }
  }, [sort, q, reload])

  return (
    <Card>
      <CardHeader>
        <CardTitle>おすすめ・最新モデル</CardTitle>
        <CardDescription>Hugging Face の最新モデルから、このマシンで動く量子化を自動で選んでワンクリックでインストールします。</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-0 flex-1 basis-48">
            <SearchIcon className="pointer-events-none absolute top-2.5 left-2.5 size-3.5 text-muted-foreground" />
            <Input className="pl-8" placeholder="モデル名で絞り込み (例: qwen3, gpt-oss)" value={input} onChange={(e) => setInput(e.target.value)} />
          </div>
          <Select value={sort} onValueChange={(v) => setSort(v as Sort)}>
            <SelectTrigger className="w-32" aria-label="並び順">
              <SelectValue>{SORT_LABEL[sort]}</SelectValue>
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(SORT_LABEL) as Sort[]).map((s) => (
                <SelectItem key={s} value={s}>
                  {SORT_LABEL[s]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button variant="outline" size="icon" title="再読み込み" disabled={loading} onClick={() => setReload((n) => n + 1)}>
            <RefreshCwIcon className={loading ? "animate-spin" : ""} />
          </Button>
        </div>

        {error && (
          <Alert variant="destructive">
            <TriangleAlertIcon />
            <AlertTitle>カタログを取得できませんでした</AlertTitle>
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}
        {!items && loading && (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
            <Loader2Icon className="size-4 animate-spin" /> 読み込み中…
          </div>
        )}
        {items?.length === 0 && !loading && <p className="py-6 text-center text-sm text-muted-foreground">該当するモデルがありません</p>}

        <div className={cn("grid gap-2 transition-opacity", loading && items && "opacity-60")}>
          {(items ?? []).map((m) => (
            <CatalogRow key={m.id} m={m} onInstall={() => setPlanRepo(m.id)} />
          ))}
        </div>
      </CardContent>
      <InstallPlanDialog repo={planRepo} onClose={() => setPlanRepo(null)} onQueued={onQueued} />
    </Card>
  )
}

function CatalogRow({ m, onInstall }: { m: CatalogItem; onInstall: () => void }) {
  const [owner, ...rest] = m.id.split("/")
  const name = rest.join("/") || owner
  const best = m.best
  return (
    <div className="grid gap-2 rounded-lg border p-3 sm:grid-cols-[1fr_auto] sm:items-center">
      <div className="min-w-0 grid gap-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span className="font-medium break-all">{name}</span>
          {rest.length > 0 && <span className="text-xs text-muted-foreground">{owner}</span>}
          {m.installed && (
            <Badge className="bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
              <CheckCircle2Icon /> インストール済み
            </Badge>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground tabular-nums">
          <span>
            <DownloadIcon className="mr-1 inline size-3" />
            {compact(m.downloads)}
          </span>
          <span>
            <HeartIcon className="mr-1 inline size-3" />
            {compact(m.likes)}
          </span>
          {m.updated != null && <span>更新 {ago(m.updated)}</span>}
          {m.pipeline_tag && <Badge variant="outline">{m.pipeline_tag}</Badge>}
        </div>
        {best ? (
          <div className="flex flex-wrap items-center gap-2 text-xs">
            <FitBadge fit={best.fit} />
            <span className="font-mono">{best.quant}</span>
            <span className="tabular-nums">{bytes(best.size)}</span>
            {best.files > 1 && <span className="text-muted-foreground">{best.files} 分割</span>}
          </div>
        ) : (
          <p className="text-xs text-muted-foreground">推奨量子化を判定できません (インストール画面で確認)</p>
        )}
        {best && (best.fit === "cpu" || best.fit === "too_big") && <p className="text-xs text-red-500">{FIT_META[best.fit].explain}</p>}
      </div>
      <Button variant={m.installed ? "outline" : "default"} onClick={onInstall} className="w-full sm:w-auto">
        <DownloadIcon /> インストール
      </Button>
    </div>
  )
}

function InstallPlanDialog({ repo, onClose, onQueued }: { repo: string | null; onClose: () => void; onQueued: () => void }) {
  const { data: fetched, error } = usePoll<CatalogPlan>(repo ? `/catalog/plan/${encRepo(repo)}` : null, 0)
  const plan = fetched && fetched.repo === repo ? fetched : undefined
  const free = useDiskFree()

  const [quant, setQuant] = useState<string | null>(null)
  const [launch, setLaunch] = useState(true)
  const [profile, setProfile] = useState(true)
  const [pending, setPending] = useState(false)

  useEffect(() => {
    if (!repo) {
      setQuant(null)
      setLaunch(true)
      setProfile(true)
    }
  }, [repo])

  // Preselect the recommended quant once the plan arrives.
  useEffect(() => {
    if (!plan) return
    const rec = plan.options.find((o) => o.quant === plan.recommended_quant) ?? plan.options.find((o) => o.fit !== "too_big")
    setQuant(rec?.quant ?? null)
  }, [plan])

  const opt = plan?.options.find((o) => o.quant === quant)
  const size = opt?.size ?? 0
  const noSpace = free != null && size > free
  const tight = free != null && !noSpace && free - size < 5 * 1024 ** 3
  const blocked = !opt || opt.fit === "too_big" || noSpace

  async function install() {
    if (!repo || !opt || blocked || pending) return
    setPending(true)
    try {
      const r = await api<{ download_id: string; quant: string; files: number }>("/catalog/install", {
        method: "POST",
        json: { repo, quant: opt.quant, launch, profile },
      })
      toast.success(`${repo} (${r.quant}) のダウンロードを開始しました`, {
        description: launch ? "完了後に自動で起動します。下のキューで進捗を確認できます。" : "下のキューで進捗を確認できます。",
      })
      onQueued()
      onClose()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open={!!repo} onOpenChange={(o) => !o && !pending && onClose()}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-xl">
        <DialogHeader>
          <DialogTitle className="break-all">{repo}</DialogTitle>
          <DialogDescription>量子化を選んでください。このマシンに最適なものが選択されています。</DialogDescription>
        </DialogHeader>

        {error && <p className="text-sm text-red-500">{error}</p>}
        {!plan && !error && (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
            <Loader2Icon className="size-4 animate-spin" /> サイズを計算中…
          </div>
        )}
        {plan && plan.options.length === 0 && <p className="text-sm text-muted-foreground">インストール可能な GGUF ファイルが見つかりません。</p>}

        {plan && plan.options.length > 0 && (
          <div role="radiogroup" aria-label="量子化" className="grid max-h-[40dvh] gap-2 overflow-y-auto pr-1">
            {plan.options.map((o) => {
              const active = quant === o.quant
              const disabled = o.fit === "too_big"
              return (
                <button
                  key={o.quant}
                  type="button"
                  role="radio"
                  aria-checked={active}
                  disabled={disabled}
                  onClick={() => setQuant(o.quant)}
                  className={cn(
                    "grid gap-1 rounded-lg border p-2.5 text-left transition-colors outline-none focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-60",
                    active ? "border-primary bg-primary/5 ring-1 ring-primary" : "enabled:hover:bg-muted/50",
                  )}
                >
                  <span className="flex flex-wrap items-center gap-2">
                    <span className={cn("grid size-4 shrink-0 place-content-center rounded-full border", active ? "border-primary bg-primary" : "border-input")}>
                      {active && <span className="size-1.5 rounded-full bg-primary-foreground" />}
                    </span>
                    <span className="font-mono text-sm font-medium">{o.quant}</span>
                    <span className="tabular-nums">{bytes(o.size)}</span>
                    <FitBadge fit={o.fit} />
                    {o.recommended && <Badge className="bg-primary/10 text-primary">おすすめ</Badge>}
                    {o.files.length > 1 && <span className="text-xs text-muted-foreground">{o.files.length} 分割</span>}
                  </span>
                  <span className="pl-6 text-xs text-muted-foreground">{o.note || FIT_META[o.fit].explain}</span>
                </button>
              )
            })}
          </div>
        )}

        {opt && (
          <div className="grid gap-2">
            <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-muted/50 px-3 py-2 text-sm">
              <span>
                ダウンロードサイズ <b className="tabular-nums">{bytes(size)}</b>
              </span>
              <span className="text-muted-foreground">
                空き容量 <span className="tabular-nums">{free != null ? bytes(free) : "不明"}</span>
              </span>
            </div>
            {noSpace && (
              <Alert variant="destructive">
                <TriangleAlertIcon />
                <AlertTitle>ディスクの空き容量が足りません</AlertTitle>
                <AlertDescription>あと {bytes(size - (free ?? 0))} 必要です。不要なモデルをアンインストールするか、より小さい量子化を選んでください。</AlertDescription>
              </Alert>
            )}
            {tight && (
              <Alert>
                <TriangleAlertIcon className="text-amber-500" />
                <AlertTitle>空き容量が少なくなります</AlertTitle>
                <AlertDescription>インストール後の空きは約 {bytes(free! - size)} です。</AlertDescription>
              </Alert>
            )}
            {(opt.fit === "cpu" || opt.fit === "partial") && (
              <p className="text-xs text-amber-700 dark:text-amber-400">{FIT_META[opt.fit].explain}</p>
            )}
          </div>
        )}

        <div className="grid gap-2">
          <Label className="gap-2 font-normal">
            <Checkbox checked={launch} onCheckedChange={(v) => setLaunch(v === true)} />
            完了後に起動する
          </Label>
          <Label className="gap-2 font-normal">
            <Checkbox checked={profile} onCheckedChange={(v) => setProfile(v === true)} />
            プロファイルとして保存
          </Label>
        </div>

        <DialogFooter>
          <Button variant="outline" disabled={pending} onClick={onClose}>
            キャンセル
          </Button>
          <Button disabled={blocked || pending} onClick={install}>
            {pending ? <Loader2Icon className="animate-spin" /> : <DownloadIcon />}
            {opt ? `${bytes(size)} をインストール` : "インストール"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
