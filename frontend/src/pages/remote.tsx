import { useEffect, useMemo, useState } from "react"
import {
  CheckCircle2Icon,
  CircleAlertIcon,
  CircleDashedIcon,
  CopyIcon,
  DownloadIcon,
  ExternalLinkIcon,
  GlobeIcon,
  Loader2Icon,
  PlayIcon,
  SaveIcon,
  SquareIcon,
  TriangleAlertIcon,
} from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { LogViewer } from "@/components/log-viewer"
import { QrImage } from "@/components/qr-image"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { api, usePoll, type ApiKey, type TunnelConfig, type TunnelProvider, type TunnelState, type TunnelStatus } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { copyText } from "@/lib/clipboard"
import { ago, dateTime } from "@/lib/format"
import { cn } from "@/lib/utils"

const MASK = "********"

const STATE_LABEL: Record<TunnelState, string> = {
  stopped: "停止中",
  installing: "インストール中",
  starting: "接続中…",
  connected: "公開中",
  degraded: "不安定",
  restarting: "再接続中",
  error: "エラー",
}
const STATE_STYLE: Record<TunnelState, string> = {
  stopped: "bg-muted text-muted-foreground",
  installing: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  starting: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  connected: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  degraded: "bg-amber-500/15 text-amber-700 dark:text-amber-400",
  restarting: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  error: "bg-red-500/15 text-red-600 dark:text-red-400",
}
const BUSY: TunnelState[] = ["installing", "starting", "restarting"]

const PROVIDERS: { id: TunnelProvider; title: string; desc: string; badges: string[] }[] = [
  { id: "cloudflare_quick", title: "Cloudflare Quick Tunnel", desc: "アカウント不要。URL は起動するたびに変わります。まず試すならこれ。", badges: ["アカウント不要", "URL 変動"] },
  { id: "cloudflare_token", title: "Cloudflare Tunnel (トークン)", desc: "固定ドメインで公開。Cloudflare Access (メール認証など) との併用を推奨。", badges: ["固定ドメイン", "Access 推奨"] },
  { id: "ngrok", title: "ngrok", desc: "authtoken が必要。無料枠でも固定ドメインを 1 つ持てます (任意)。", badges: ["authtoken", "固定ドメイン任意"] },
]

function binaryFor(t: TunnelStatus, p: TunnelProvider) {
  return p === "ngrok" ? t.binaries.ngrok : t.binaries.cloudflared
}

function configProblem(c: TunnelConfig): string | null {
  if (c.provider === "cloudflare_token" && !c.cloudflare_token) return "Cloudflare のトンネルトークンを入力してください"
  if (c.provider === "ngrok" && !c.ngrok_authtoken) return "ngrok の authtoken を入力してください"
  return null
}

export function RemotePage() {
  const { data: t, refetch } = usePoll<TunnelStatus>("/tunnel", 2000)
  const { data: keys } = usePoll<ApiKey[]>("/auth/keys", 30000)
  const { status: auth } = useAuth()

  const [form, setForm] = useState<TunnelConfig | null>(null)
  const [saving, setSaving] = useState(false)
  const [acting, setActing] = useState<"start" | "install" | null>(null)
  const [stopOpen, setStopOpen] = useState(false)

  // Initialise the form once; later polls must not overwrite what the user is typing.
  useEffect(() => {
    if (t && !form) setForm(t.config)
  }, [t, form])

  const dirty = useMemo(() => !!t && !!form && JSON.stringify(t.config) !== JSON.stringify(form), [t, form])

  if (!t || !form) {
    return (
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2Icon className="size-4 animate-spin" /> 読み込み中…
      </div>
    )
  }

  const set = <K extends keyof TunnelConfig>(k: K, v: TunnelConfig[K]) => setForm((f) => (f ? { ...f, [k]: v } : f))
  const bin = binaryFor(t, form.provider)
  const running = t.state !== "stopped" && t.state !== "error"
  const busy = BUSY.includes(t.state)
  const savedBin = binaryFor(t, t.config.provider)
  const problem = configProblem(t.config)
  const formProblem = configProblem(form)
  const viaTunnel = !!t.public_url && (() => {
    try {
      return new URL(t.public_url).hostname === location.hostname
    } catch {
      return false
    }
  })()

  const startBlock: string | null = !t.auth_configured
    ? "先に認証 (管理者パスワード) を設定してください。認証なしで公開すると誰でも操作できてしまいます。"
    : dirty
      ? "未保存の変更があります。先に「保存」してください。"
      : problem
        ? problem
        : !savedBin
          ? "必要なバイナリが未インストールです。「インストール」を押してください。"
          : null

  async function save() {
    if (!form || saving) return
    setSaving(true)
    try {
      await api("/tunnel/config", { method: "PUT", json: form })
      toast.success("保存しました")
      const fresh = await api<TunnelStatus>("/tunnel")
      setForm(fresh.config)
      await refetch()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  async function start() {
    if (acting) return
    setActing("start")
    try {
      await api("/tunnel/start", { method: "POST" })
      toast.success("トンネルを開始しました")
      await refetch()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setActing(null)
    }
  }

  async function install() {
    if (acting || !form) return
    setActing("install")
    try {
      await api("/tunnel/install", { method: "POST", json: { provider: form.provider } })
      toast.success("インストールを開始しました (ログを確認してください)")
      await refetch()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setActing(null)
    }
  }

  return (
    <div className="grid max-w-5xl gap-4">
      {/* ------------------------------------------------------------ status */}
      <Card>
        <CardHeader>
          <CardTitle className="flex flex-wrap items-center gap-2">
            <GlobeIcon className="size-4" /> 公開状態
            <Badge variant="secondary" className={cn("gap-1.5 border-0", STATE_STYLE[t.state])}>
              {(busy || t.state === "connected") && <span className="size-1.5 animate-pulse rounded-full bg-current" />}
              {STATE_LABEL[t.state]}
            </Badge>
          </CardTitle>
          <CardDescription>外出先のスマホなどから、ポート開放なしでこのパネルと OpenAI 互換 API (/v1) にアクセスできます。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          {t.public_url ? (
            <div className="grid gap-4 sm:grid-cols-[1fr_auto] sm:items-start">
              <div className="grid min-w-0 gap-3">
                <div className="grid gap-1">
                  <span className="text-xs text-muted-foreground">公開 URL</span>
                  <a href={t.public_url} target="_blank" rel="noreferrer" className="text-lg font-semibold break-all text-sky-600 underline-offset-2 hover:underline sm:text-xl dark:text-sky-400">
                    {t.public_url}
                  </a>
                </div>
                <div className="flex flex-wrap gap-2">
                  <Button variant="outline" onClick={() => void copyText(t.public_url!, "URL をコピーしました")}>
                    <CopyIcon /> コピー
                  </Button>
                  <Button variant="outline" nativeButton={false} render={<a href={t.public_url} target="_blank" rel="noreferrer" />}>
                    <ExternalLinkIcon /> 開く
                  </Button>
                </div>
                <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm sm:grid-cols-3">
                  <Fact k="接続開始" v={t.connected_since ? `${ago(t.connected_since)}` : "—"} title={dateTime(t.connected_since)} />
                  <Fact k="再起動回数" v={String(t.restarts)} />
                  <Fact
                    k="ヘルスチェック"
                    v={t.last_health ? (t.last_health.ok ? `OK · ${Math.round(t.last_health.latency_ms)} ms` : "失敗") : "—"}
                    tone={t.last_health ? (t.last_health.ok ? "ok" : "bad") : undefined}
                    title={t.last_health ? dateTime(t.last_health.at) : undefined}
                  />
                </dl>
              </div>
              <div className="grid justify-items-center gap-1">
                <QrImage value={t.public_url} size={160} />
                <span className="text-xs text-muted-foreground">スマホで読み取る</span>
              </div>
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">{t.state === "stopped" ? "公開していません。下の設定を確認して「開始」を押してください。" : "公開 URL を取得中…"}</p>
          )}

          {t.last_error && (
            <Alert variant="destructive">
              <CircleAlertIcon />
              <AlertTitle>直近のエラー</AlertTitle>
              <AlertDescription className="break-all">{t.last_error}</AlertDescription>
            </Alert>
          )}
          {t.public_url && !auth.totp_enabled && (
            <Alert>
              <TriangleAlertIcon className="text-amber-500" />
              <AlertTitle>二段階認証が未設定です</AlertTitle>
              <AlertDescription>
                インターネットに公開中です。<a href="#security" className="underline">二段階認証を有効にする</a>ことを強くおすすめします。
              </AlertDescription>
            </Alert>
          )}

          <div className="flex flex-wrap items-center gap-2 border-t pt-4">
            <Button onClick={start} disabled={running || busy || !!startBlock || acting !== null}>
              {acting === "start" ? <Loader2Icon className="animate-spin" /> : <PlayIcon />} 開始
            </Button>
            <Button variant="destructive" onClick={() => setStopOpen(true)} disabled={!running}>
              <SquareIcon /> 停止
            </Button>
            {!running && startBlock && <p className="basis-full text-xs text-amber-700 dark:text-amber-400">{startBlock}</p>}
          </div>
        </CardContent>
      </Card>

      {/* ---------------------------------------------------------- settings */}
      <Card>
        <CardHeader>
          <CardTitle>接続方法</CardTitle>
          <CardDescription>変更したら「保存」してから「開始」してください。稼働中の変更は停止 → 開始で反映されます。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          <div role="radiogroup" aria-label="トンネルの種類" className="grid gap-2 md:grid-cols-3">
            {PROVIDERS.map((p) => {
              const active = form.provider === p.id
              return (
                <button
                  key={p.id}
                  type="button"
                  role="radio"
                  aria-checked={active}
                  onClick={() => set("provider", p.id)}
                  className={cn(
                    "grid content-start gap-1.5 rounded-lg border p-3 text-left transition-colors outline-none focus-visible:ring-3 focus-visible:ring-ring/50",
                    active ? "border-primary bg-primary/5 ring-1 ring-primary" : "hover:bg-muted/50",
                  )}
                >
                  <span className="flex items-center gap-2 font-medium">
                    <span className={cn("grid size-4 shrink-0 place-content-center rounded-full border", active ? "border-primary bg-primary" : "border-input")}>
                      {active && <span className="size-1.5 rounded-full bg-primary-foreground" />}
                    </span>
                    {p.title}
                  </span>
                  <span className="text-xs text-muted-foreground">{p.desc}</span>
                  <span className="flex flex-wrap gap-1">
                    {p.badges.map((b) => (
                      <Badge key={b} variant="outline">
                        {b}
                      </Badge>
                    ))}
                  </span>
                </button>
              )
            })}
          </div>

          {form.provider === "cloudflare_token" && (
            <div className="grid max-w-xl gap-4">
              <SecretField id="cf-token" label="トンネルトークン" value={form.cloudflare_token} onChange={(v) => set("cloudflare_token", v)} hint="Cloudflare Zero Trust > Networks > Tunnels で作成したトンネルのトークン。保存後は伏せ字で表示されます。" />
              <div className="grid gap-1.5">
                <Label htmlFor="cf-host">公開ホスト名 (任意)</Label>
                <Input id="cf-host" placeholder="llm.example.com" autoCapitalize="none" value={form.cloudflare_hostname} onChange={(e) => set("cloudflare_hostname", e.target.value.trim())} />
                <p className="text-xs text-muted-foreground">トンネルに割り当てたドメイン。入力すると公開 URL の表示に使われます。</p>
              </div>
            </div>
          )}
          {form.provider === "ngrok" && (
            <div className="grid max-w-xl gap-4">
              <SecretField id="ng-token" label="authtoken" value={form.ngrok_authtoken} onChange={(v) => set("ngrok_authtoken", v)} hint="ngrok ダッシュボードの Your Authtoken。保存後は伏せ字で表示されます。" />
              <div className="grid gap-1.5">
                <Label htmlFor="ng-domain">固定ドメイン (任意)</Label>
                <Input id="ng-domain" placeholder="example.ngrok-free.app" autoCapitalize="none" value={form.ngrok_domain} onChange={(e) => set("ngrok_domain", e.target.value.trim())} />
              </div>
            </div>
          )}

          <div className="flex flex-wrap items-center gap-2 rounded-lg border p-3 text-sm">
            {bin ? <CheckCircle2Icon className="size-4 text-emerald-500" /> : <CircleDashedIcon className="size-4 text-muted-foreground" />}
            <span className="font-medium">{form.provider === "ngrok" ? "ngrok" : "cloudflared"}</span>
            <span className="text-muted-foreground">{bin ? `インストール済み${bin.version ? ` (${bin.version})` : ""}` : "未インストール"}</span>
            {!bin && (
              <Button size="sm" className="ml-auto" onClick={install} disabled={acting !== null || t.state === "installing"}>
                {acting === "install" || t.state === "installing" ? <Loader2Icon className="animate-spin" /> : <DownloadIcon />} インストール
              </Button>
            )}
          </div>

          <div className="flex items-center gap-3">
            <Switch id="tn-auto" checked={form.autostart} onCheckedChange={(v) => set("autostart", v)} />
            <Label htmlFor="tn-auto" className="font-normal">
              ハーネス起動時に自動でトンネルを開始する
            </Label>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <Button onClick={save} disabled={!dirty || saving || !!formProblem}>
              {saving ? <Loader2Icon className="animate-spin" /> : <SaveIcon />} 保存
            </Button>
            {dirty && <span className="text-xs text-amber-700 dark:text-amber-400">未保存の変更があります</span>}
            {formProblem && <span className="text-xs text-red-500">{formProblem}</span>}
          </div>
        </CardContent>
      </Card>

      {/* --------------------------------------------------------- security */}
      <Card>
        <CardHeader>
          <CardTitle>セキュリティチェックリスト</CardTitle>
          <CardDescription>インターネットに公開する前に確認してください。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-2">
          <Check ok={t.auth_configured} title="認証設定済み" desc={t.auth_configured ? "管理者アカウントが設定されています。" : "ワークステーション上で管理者アカウントを作成してください。未設定のうちは公開できません。"} />
          <Check
            ok={auth.totp_enabled}
            title="二段階認証 (TOTP)"
            desc={auth.totp_enabled ? "有効です。" : "未設定です。パスワード漏洩時の被害を防ぐため強く推奨します。"}
            action={!auth.totp_enabled ? <a href="#security" className="text-sm text-sky-600 underline-offset-2 hover:underline dark:text-sky-400">有効にする</a> : undefined}
          />
          <Check
            ok={keys ? keys.length > 0 : undefined}
            neutral
            title="API キー"
            desc={keys ? (keys.length > 0 ? `${keys.length} 件発行済み。不要なキーは削除してください。` : "外部のアプリから /v1 を使うなら API キーを発行してください (管理画面とは別の権限です)。") : "確認中…"}
            action={<a href="#security" className="text-sm text-sky-600 underline-offset-2 hover:underline dark:text-sky-400">管理</a>}
          />
          <Check neutral title="Cloudflare Access でさらに安全に" desc="Cloudflare Access でメール認証 (ワンタイムコード) を追加すると、パスワード画面に到達する前に不正アクセスを遮断できます。固定ドメインのトンネルで利用できます。" />
          <Check neutral title="公開 URL は推測されにくい場所にだけ共有" desc="Quick Tunnel の URL は毎回変わりますが、知っている人は誰でもログイン画面を開けます。" />
        </CardContent>
      </Card>

      {/* -------------------------------------------------------------- logs */}
      <Card>
        <CardHeader>
          <CardTitle>トンネルログ</CardTitle>
        </CardHeader>
        <CardContent>
          <LogViewer path="/tunnel/logs" className="h-64 sm:h-80" />
        </CardContent>
      </Card>

      <ConfirmDialog
        open={stopOpen}
        onOpenChange={setStopOpen}
        title="トンネルを停止しますか?"
        description="外部からのアクセスがすべて遮断されます。"
        confirmLabel="停止する"
        onConfirm={async () => {
          await api("/tunnel/stop", { method: "POST" })
          toast.success("トンネルを停止しました")
          await refetch()
        }}
      >
        {viaTunnel ? (
          <Alert variant="destructive">
            <TriangleAlertIcon />
            <AlertTitle>今この画面はトンネル経由で開いています</AlertTitle>
            <AlertDescription>停止するとこの端末からもパネルに接続できなくなります。再開するにはワークステーション上で操作が必要です。</AlertDescription>
          </Alert>
        ) : (
          <p className="text-sm text-muted-foreground">再開するには「開始」を押してください。Quick Tunnel の場合は URL が変わります。</p>
        )}
      </ConfirmDialog>
    </div>
  )
}

function Fact({ k, v, tone, title }: { k: string; v: string; tone?: "ok" | "bad"; title?: string }) {
  return (
    <div title={title}>
      <dt className="text-xs text-muted-foreground">{k}</dt>
      <dd className={cn("font-medium tabular-nums", tone === "ok" && "text-emerald-600 dark:text-emerald-400", tone === "bad" && "text-red-500")}>{v}</dd>
    </div>
  )
}

function SecretField({ id, label, value, onChange, hint }: { id: string; label: string; value: string; onChange: (v: string) => void; hint?: string }) {
  const masked = value === MASK
  return (
    <div className="grid gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      <div className="flex gap-2">
        <Input
          id={id}
          type="password"
          autoComplete="off"
          autoCapitalize="none"
          spellCheck={false}
          value={value}
          // Selecting the mask clears it so a new secret can be typed; leaving it untouched keeps the stored one.
          onFocus={(e) => masked && e.currentTarget.select()}
          onChange={(e) => onChange(e.target.value)}
          placeholder="未設定"
        />
        {masked && (
          <Button type="button" variant="outline" onClick={() => onChange("")}>
            変更
          </Button>
        )}
      </div>
      <p className="text-xs text-muted-foreground">{masked ? "設定済み (そのままなら変更されません)。" : hint}</p>
    </div>
  )
}

function Check({ ok, neutral, title, desc, action }: { ok?: boolean; neutral?: boolean; title: string; desc: string; action?: React.ReactNode }) {
  const Icon = ok ? CheckCircle2Icon : neutral ? CircleDashedIcon : TriangleAlertIcon
  const color = ok ? "text-emerald-500" : neutral ? "text-muted-foreground" : "text-amber-500"
  return (
    <div className="flex items-start gap-3 rounded-lg border p-3">
      <Icon className={cn("mt-0.5 size-4 shrink-0", color)} />
      <div className="min-w-0 flex-1">
        <div className="font-medium">{title}</div>
        <p className="text-xs text-muted-foreground">{desc}</p>
      </div>
      {action}
    </div>
  )
}
