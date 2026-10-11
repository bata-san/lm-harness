import { useEffect, useState } from "react"
import { CheckIcon, CopyIcon, KeyRoundIcon, Loader2Icon, MonitorSmartphoneIcon, PlusIcon, ShieldCheckIcon, SmartphoneIcon, Trash2Icon, TriangleAlertIcon } from "lucide-react"
import { toast } from "sonner"

import { CodeBlock } from "@/components/code-block"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { PasswordInput, StrengthBar } from "@/components/password-input"
import { QrImage } from "@/components/qr-image"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, usePoll, type ApiKey, type AuthSession } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { copyText } from "@/lib/clipboard"
import { ago, dateTime } from "@/lib/format"

export function SecurityPage() {
  return (
    <div className="grid max-w-4xl gap-4">
      <PasswordCard />
      <TotpCard />
      <SessionsCard />
      <ApiKeysCard />
    </div>
  )
}

/* ------------------------------------------------------------------ password */

function PasswordCard() {
  const [cur, setCur] = useState("")
  const [next, setNext] = useState("")
  const [next2, setNext2] = useState("")
  const [pending, setPending] = useState(false)

  const mismatch = next2.length > 0 && next !== next2
  const valid = cur.length > 0 && next.length >= 12 && next === next2 && next !== cur

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!valid || pending) return
    setPending(true)
    try {
      await api("/auth/password", { method: "POST", json: { current: cur, new: next } })
      toast.success("パスワードを変更しました")
      setCur("")
      setNext("")
      setNext2("")
    } catch (err) {
      toast.error((err as Error).message)
    } finally {
      setPending(false)
    }
  }

  return (
    <Card id="password">
      <CardHeader>
        <CardTitle>パスワード変更</CardTitle>
        <CardDescription>12 文字以上。変更後も他の端末のセッションは有効なままなので、不審な場合は下のセッション一覧から切断してください。</CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="grid max-w-md gap-4">
          <div className="grid gap-1.5">
            <Label htmlFor="pw-cur">現在のパスワード</Label>
            <PasswordInput id="pw-cur" autoComplete="current-password" value={cur} onChange={(e) => setCur(e.target.value)} />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="pw-new">新しいパスワード</Label>
            <PasswordInput id="pw-new" autoComplete="new-password" value={next} onChange={(e) => setNext(e.target.value)} />
            <StrengthBar password={next} />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="pw-new2">新しいパスワード (確認)</Label>
            <PasswordInput id="pw-new2" autoComplete="new-password" value={next2} onChange={(e) => setNext2(e.target.value)} aria-invalid={mismatch} />
            {mismatch && <p className="text-xs text-red-500">パスワードが一致しません</p>}
          </div>
          <div>
            <Button type="submit" disabled={!valid || pending}>
              {pending && <Loader2Icon className="animate-spin" />} パスワードを変更
            </Button>
          </div>
        </form>
      </CardContent>
    </Card>
  )
}

/* ---------------------------------------------------------------------- TOTP */

function TotpCard() {
  const { status, refresh } = useAuth()
  const [setupOpen, setSetupOpen] = useState(false)
  const [disableOpen, setDisableOpen] = useState(false)

  return (
    <Card id="totp">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          二段階認証 (TOTP)
          {status.totp_enabled ? (
            <Badge className="bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">有効</Badge>
          ) : (
            <Badge className="bg-amber-500/15 text-amber-700 dark:text-amber-400">未設定</Badge>
          )}
        </CardTitle>
        <CardDescription>Google Authenticator / 1Password / Authy などの認証アプリで 6 桁コードを生成します。インターネット公開するなら必ず有効にしてください。</CardDescription>
      </CardHeader>
      <CardContent>
        {status.totp_enabled ? (
          <Button variant="destructive" onClick={() => setDisableOpen(true)}>
            二段階認証を無効にする
          </Button>
        ) : (
          <Button onClick={() => setSetupOpen(true)}>
            <ShieldCheckIcon /> 二段階認証を設定する
          </Button>
        )}
      </CardContent>
      <TotpSetupDialog
        open={setupOpen}
        onOpenChange={setSetupOpen}
        onEnabled={async () => {
          await refresh()
        }}
      />
      <TotpDisableDialog
        open={disableOpen}
        onOpenChange={setDisableOpen}
        onDisabled={async () => {
          await refresh()
        }}
      />
    </Card>
  )
}

function TotpSetupDialog({ open, onOpenChange, onEnabled }: { open: boolean; onOpenChange: (o: boolean) => void; onEnabled: () => Promise<void> }) {
  const [info, setInfo] = useState<{ secret: string; otpauth_url: string } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [code, setCode] = useState("")
  const [pending, setPending] = useState(false)

  useEffect(() => {
    if (!open) {
      setInfo(null)
      setError(null)
      setCode("")
      return
    }
    let alive = true
    api<{ secret: string; otpauth_url: string }>("/auth/totp/setup", { method: "POST" })
      .then((r) => alive && setInfo(r))
      .catch((e) => alive && setError((e as Error).message))
    return () => {
      alive = false
    }
  }, [open])

  async function verify(e: React.FormEvent) {
    e.preventDefault()
    if (pending || code.length < 6) return
    setPending(true)
    try {
      await api("/auth/totp/enable", { method: "POST", json: { code } })
      toast.success("二段階認証を有効にしました")
      await onEnabled()
      onOpenChange(false)
    } catch (err) {
      toast.error((err as Error).message)
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !pending && onOpenChange(o)}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>二段階認証の設定</DialogTitle>
          <DialogDescription>1. 認証アプリで QR コードを読み取る (スマホで開いている場合は下のシークレットを手入力)。2. 表示された 6 桁コードを入力。</DialogDescription>
        </DialogHeader>
        {error && <p className="text-sm text-red-500">{error}</p>}
        {!info && !error && (
          <div className="flex justify-center py-8 text-muted-foreground">
            <Loader2Icon className="animate-spin" />
          </div>
        )}
        {info && (
          <form onSubmit={verify} className="grid gap-4">
            <div className="flex justify-center">
              <QrImage value={info.otpauth_url} size={176} />
            </div>
            <div className="grid gap-1.5">
              <Label>シークレット (手入力用)</Label>
              <div className="flex items-center gap-1">
                <code className="min-w-0 flex-1 rounded-lg border bg-muted/50 px-2 py-1.5 font-mono text-xs break-all select-all">{info.secret}</code>
                <Button type="button" size="icon" variant="outline" title="コピー" onClick={() => void copyText(info.secret)}>
                  <CopyIcon />
                </Button>
              </div>
              <a href={info.otpauth_url} className="text-xs text-sky-600 underline-offset-2 hover:underline dark:text-sky-400">
                このデバイスの認証アプリで開く
              </a>
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="totp-code">確認コード</Label>
              <Input
                id="totp-code"
                inputMode="numeric"
                autoComplete="one-time-code"
                placeholder="123456"
                maxLength={8}
                value={code}
                onChange={(e) => setCode(e.target.value.replace(/\s/g, ""))}
                className="text-center font-mono text-lg tracking-widest"
              />
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" disabled={pending} onClick={() => onOpenChange(false)}>
                キャンセル
              </Button>
              <Button type="submit" disabled={pending || code.length < 6}>
                {pending && <Loader2Icon className="animate-spin" />} 確認して有効化
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}

function TotpDisableDialog({ open, onOpenChange, onDisabled }: { open: boolean; onOpenChange: (o: boolean) => void; onDisabled: () => Promise<void> }) {
  const [pw, setPw] = useState("")
  useEffect(() => {
    if (!open) setPw("")
  }, [open])
  return (
    <ConfirmDialog
      open={open}
      onOpenChange={onOpenChange}
      title="二段階認証を無効にしますか?"
      description="パスワードだけでログインできるようになり、インターネット公開時の安全性が下がります。"
      confirmLabel="無効にする"
      disabled={!pw}
      onConfirm={async () => {
        await api("/auth/totp/disable", { method: "POST", json: { password: pw } })
        toast.success("二段階認証を無効にしました")
        await onDisabled()
      }}
    >
      <div className="grid gap-1.5">
        <Label htmlFor="totp-dis-pw">パスワードで本人確認</Label>
        <PasswordInput id="totp-dis-pw" autoComplete="current-password" value={pw} onChange={(e) => setPw(e.target.value)} />
      </div>
    </ConfirmDialog>
  )
}

/* ------------------------------------------------------------------ sessions */

function shortUA(ua: string): string {
  if (!ua) return "不明な端末"
  const os = /iPhone|iPad/.test(ua) ? "iOS" : /Android/.test(ua) ? "Android" : /Windows/.test(ua) ? "Windows" : /Mac OS X/.test(ua) ? "macOS" : /Linux/.test(ua) ? "Linux" : ""
  const br = /Edg\//.test(ua) ? "Edge" : /OPR\//.test(ua) ? "Opera" : /Firefox\//.test(ua) ? "Firefox" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : /curl|python|node/i.test(ua) ? ua.split(" ")[0] : ""
  return [br, os].filter(Boolean).join(" / ") || ua.slice(0, 40)
}

function SessionsCard() {
  const { data, refetch } = usePoll<AuthSession[]>("/auth/sessions", 15000)
  const [target, setTarget] = useState<AuthSession | null>(null)
  const { refresh } = useAuth()

  return (
    <Card id="sessions">
      <CardHeader>
        <CardTitle>ログイン中のセッション</CardTitle>
        <CardDescription>身に覚えのないセッションは切断してください。</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-2">
        {!data && <p className="text-sm text-muted-foreground">読み込み中…</p>}
        {(data ?? []).map((s) => {
          const mobile = /iPhone|Android|Mobile/.test(s.user_agent)
          const Icon = mobile ? SmartphoneIcon : MonitorSmartphoneIcon
          return (
            <div key={s.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border p-3">
              <Icon className="size-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium" title={s.user_agent}>
                    {shortUA(s.user_agent)}
                  </span>
                  {s.current && <Badge className="bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">この端末</Badge>}
                </div>
                <div className="text-xs text-muted-foreground">
                  {s.ip} · 最終利用 {ago(s.last_seen_at)} · ログイン {dateTime(s.created_at)}
                </div>
              </div>
              <Button size="sm" variant="destructive" onClick={() => setTarget(s)}>
                {s.current ? "ログアウト" : "切断"}
              </Button>
            </div>
          )
        })}
      </CardContent>
      <ConfirmDialog
        open={!!target}
        onOpenChange={(o) => !o && setTarget(null)}
        title={target?.current ? "この端末をログアウトしますか?" : "このセッションを切断しますか?"}
        description={target ? `${shortUA(target.user_agent)} (${target.ip})` : undefined}
        confirmLabel={target?.current ? "ログアウト" : "切断する"}
        onConfirm={async () => {
          if (!target) return
          await api(`/auth/sessions/${target.id}`, { method: "DELETE" })
          toast.success(target.current ? "ログアウトしました" : "セッションを切断しました")
          if (target.current) await refresh()
          else await refetch()
        }}
      >
        <p className="text-sm text-muted-foreground">{target?.current ? "再度ログインが必要になります。" : "その端末は次のアクセスでログイン画面に戻ります。"}</p>
      </ConfirmDialog>
    </Card>
  )
}

/* ------------------------------------------------------------------ API keys */

export function ApiUsage({ keyText }: { keyText: string }) {
  const origin = location.origin
  return (
    <div className="grid gap-3">
      <div className="grid gap-1">
        <span className="text-xs font-medium text-muted-foreground">curl</span>
        <CodeBlock code={`curl ${origin}/v1/models \\\n  -H "Authorization: Bearer ${keyText}"`} />
      </div>
      <div className="grid gap-1">
        <span className="text-xs font-medium text-muted-foreground">OpenAI SDK (Python)</span>
        <CodeBlock code={`from openai import OpenAI\n\nclient = OpenAI(\n    base_url="${origin}/v1",\n    api_key="${keyText}",\n)`} />
      </div>
    </div>
  )
}

function ApiKeysCard() {
  const { data, refetch } = usePoll<ApiKey[]>("/auth/keys", 15000)
  const [createOpen, setCreateOpen] = useState(false)
  const [created, setCreated] = useState<{ id: string; name: string; key: string } | null>(null)
  const [del, setDel] = useState<ApiKey | null>(null)

  return (
    <Card id="keys">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <KeyRoundIcon className="size-4" /> API キー
        </CardTitle>
        <CardDescription>
          OpenAI 互換プロキシ <code className="font-mono">{location.origin}/v1</code> を外部のアプリ / スクリプトから使うためのキー。パネルの管理画面には使えません。
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">
        <div className="grid gap-2">
          {data?.length === 0 && <p className="text-sm text-muted-foreground">API キーはまだありません</p>}
          {(data ?? []).map((k) => (
            <div key={k.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border p-3">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{k.name}</span>
                  <code className="font-mono text-xs text-muted-foreground">{k.prefix}…</code>
                </div>
                <div className="text-xs text-muted-foreground">
                  作成 {dateTime(k.created_at)} · {k.last_used_at ? `最終利用 ${ago(k.last_used_at)}` : "未使用"}
                </div>
              </div>
              <Button size="icon-sm" variant="ghost" title="削除" aria-label={`${k.name} を削除`} onClick={() => setDel(k)}>
                <Trash2Icon />
              </Button>
            </div>
          ))}
        </div>
        <div>
          <Button onClick={() => setCreateOpen(true)}>
            <PlusIcon /> API キーを作成
          </Button>
        </div>
        <ApiUsage keyText="lh_..." />
      </CardContent>

      <CreateKeyDialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        onCreated={(k) => {
          setCreated(k)
          void refetch()
        }}
      />
      <KeyShownDialog created={created} onClose={() => setCreated(null)} />
      <ConfirmDialog
        open={!!del}
        onOpenChange={(o) => !o && setDel(null)}
        title="API キーを削除しますか?"
        description={del ? `「${del.name}」(${del.prefix}…)` : undefined}
        confirmLabel="削除する"
        onConfirm={async () => {
          if (!del) return
          await api(`/auth/keys/${del.id}`, { method: "DELETE" })
          toast.success("API キーを削除しました")
          await refetch()
        }}
      >
        <p className="text-sm text-muted-foreground">このキーを使っているアプリは直ちにアクセスできなくなります。元に戻せません。</p>
      </ConfirmDialog>
    </Card>
  )
}

function CreateKeyDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange: (o: boolean) => void; onCreated: (k: { id: string; name: string; key: string }) => void }) {
  const [name, setName] = useState("")
  const [pending, setPending] = useState(false)
  useEffect(() => {
    if (!open) setName("")
  }, [open])

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!name.trim() || pending) return
    setPending(true)
    try {
      const k = await api<{ id: string; name: string; key: string }>("/auth/keys", { method: "POST", json: { name: name.trim() } })
      onOpenChange(false)
      onCreated(k)
    } catch (err) {
      toast.error((err as Error).message)
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !pending && onOpenChange(o)}>
      <DialogContent>
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>API キーを作成</DialogTitle>
            <DialogDescription>どこで使うキーか分かる名前を付けてください (例: スマホアプリ, VSCode)。</DialogDescription>
          </DialogHeader>
          <div className="grid gap-1.5">
            <Label htmlFor="key-name">名前</Label>
            <Input id="key-name" autoFocus maxLength={64} value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" disabled={pending} onClick={() => onOpenChange(false)}>
              キャンセル
            </Button>
            <Button type="submit" disabled={!name.trim() || pending}>
              {pending && <Loader2Icon className="animate-spin" />} 作成
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

/** Shows a freshly created key exactly once; can only be closed after an explicit acknowledgement. */
function KeyShownDialog({ created, onClose }: { created: { id: string; name: string; key: string } | null; onClose: () => void }) {
  const [ack, setAck] = useState(false)
  useEffect(() => {
    setAck(false)
  }, [created])
  return (
    <Dialog open={!!created} onOpenChange={(o) => !o && ack && onClose()}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-lg" showCloseButton={false}>
        <DialogHeader>
          <DialogTitle>API キーを作成しました</DialogTitle>
          <DialogDescription>「{created?.name}」</DialogDescription>
        </DialogHeader>
        <Alert variant="destructive">
          <TriangleAlertIcon />
          <AlertTitle>このキーは今しか表示されません</AlertTitle>
          <AlertDescription>画面を閉じると二度と確認できません。今コピーして安全な場所 (パスワードマネージャー等) に保存してください。</AlertDescription>
        </Alert>
        <div className="flex items-center gap-1">
          <code className="min-w-0 flex-1 rounded-lg border bg-muted/50 px-2 py-2 font-mono text-xs break-all select-all">{created?.key}</code>
          <Button type="button" size="icon" variant="outline" title="コピー" onClick={() => created && void copyText(created.key, "API キーをコピーしました")}>
            <CopyIcon />
          </Button>
        </div>
        {created && <ApiUsage keyText={created.key} />}
        <Label className="items-start gap-2 font-normal">
          <Checkbox checked={ack} onCheckedChange={(v) => setAck(v === true)} className="mt-0.5" />
          <span>キーを安全な場所に保存しました</span>
        </Label>
        <DialogFooter>
          <Button disabled={!ack} onClick={onClose}>
            <CheckIcon /> 閉じる
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
