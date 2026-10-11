import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react"
import { Loader2Icon, LockKeyholeIcon, MonitorIcon, RefreshCwIcon, ShieldAlertIcon } from "lucide-react"
import { toast } from "sonner"

import { PasswordInput, StrengthBar } from "@/components/password-input"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, ApiError, onUnauthorized, type AuthStatus } from "@/lib/api"

type Ctx = {
  status: AuthStatus
  refresh: () => Promise<void>
  logout: () => Promise<void>
}
const AuthContext = createContext<Ctx | null>(null)

// eslint-disable-next-line react-refresh/only-export-components
export function useAuth(): Ctx {
  const c = useContext(AuthContext)
  if (!c) throw new Error("useAuth outside AuthGate")
  return c
}

function Frame({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-svh items-center justify-center bg-muted/30 p-4">
      <div className="grid w-full max-w-sm gap-4">
        <div className="flex items-center justify-center gap-2">
          <img src="/favicon.svg" className="size-8" alt="" />
          <span className="text-lg font-semibold">LLM Harness</span>
        </div>
        {children}
      </div>
    </div>
  )
}

/** Renders the app only for an authenticated session; otherwise the setup / login screen (hash route is preserved). */
export function AuthGate({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<AuthStatus | undefined>()
  const [loadError, setLoadError] = useState<string | undefined>()
  const [loading, setLoading] = useState(false)
  const inflight = useRef<Promise<void> | null>(null)

  const refresh = useCallback(async () => {
    if (inflight.current) return inflight.current
    const p = (async () => {
      setLoading(true)
      try {
        setStatus(await api<AuthStatus>("/auth/status"))
        setLoadError(undefined)
      } catch (e) {
        setLoadError((e as Error).message)
      } finally {
        setLoading(false)
        inflight.current = null
      }
    })()
    inflight.current = p
    return p
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  // Any API call answering 401 (expired / revoked session) re-checks status; the hash route is left untouched.
  useEffect(() => onUnauthorized(() => void refresh()), [refresh])

  const logout = useCallback(async () => {
    try {
      await api("/auth/logout", { method: "POST" })
    } catch {
      /* already gone */
    }
    await refresh()
  }, [refresh])

  if (!status) {
    return (
      <Frame>
        {loadError ? (
          <Alert variant="destructive">
            <ShieldAlertIcon />
            <AlertTitle>サーバに接続できません</AlertTitle>
            <AlertDescription>
              <p>{loadError}</p>
              <Button size="sm" variant="outline" className="mt-2" onClick={() => void refresh()} disabled={loading}>
                {loading ? <Loader2Icon className="animate-spin" /> : <RefreshCwIcon />} 再試行
              </Button>
            </AlertDescription>
          </Alert>
        ) : (
          <div className="flex justify-center py-8 text-muted-foreground">
            <Loader2Icon className="animate-spin" />
          </div>
        )}
      </Frame>
    )
  }

  if (!status.configured) {
    return <Frame>{status.local ? <SetupCard onDone={refresh} /> : <NotLocalCard />}</Frame>
  }
  if (!status.authenticated) {
    return (
      <Frame>
        <LoginCard onDone={refresh} />
      </Frame>
    )
  }
  return <AuthContext.Provider value={{ status, refresh, logout }}>{children}</AuthContext.Provider>
}

function NotLocalCard() {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <MonitorIcon className="size-4" /> 初期設定が必要です
        </CardTitle>
        <CardDescription>ワークステーション上で初期設定してください</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-2 text-sm text-muted-foreground">
        <p>安全のため、管理者アカウントの作成はこのパネルを動かしているワークステーション自身のブラウザ (http://localhost) からのみ行えます。</p>
        <p>設定が済んだらこの画面を再読み込みしてください。</p>
      </CardContent>
    </Card>
  )
}

function SetupCard({ onDone }: { onDone: () => Promise<void> }) {
  const [username, setUsername] = useState("admin")
  const [pw, setPw] = useState("")
  const [pw2, setPw2] = useState("")
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const mismatch = pw2.length > 0 && pw !== pw2
  const valid = username.trim().length > 0 && pw.length >= 12 && pw === pw2

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!valid || pending) return
    setPending(true)
    setError(null)
    try {
      await api("/auth/setup", { method: "POST", json: { username: username.trim(), password: pw } })
      toast.success("管理者アカウントを作成しました")
      await onDone()
    } catch (err) {
      setError((err as Error).message)
    } finally {
      setPending(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <LockKeyholeIcon className="size-4" /> 初期設定
        </CardTitle>
        <CardDescription>管理者アカウントを作成します。この設定はインターネット公開 (リモートアクセス) を始める前に必須です。</CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="grid gap-4">
          <div className="grid gap-1.5">
            <Label htmlFor="su-user">ユーザー名</Label>
            <Input id="su-user" autoComplete="username" value={username} onChange={(e) => setUsername(e.target.value)} />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="su-pw">パスワード (12 文字以上)</Label>
            <PasswordInput id="su-pw" autoComplete="new-password" value={pw} onChange={(e) => setPw(e.target.value)} />
            <StrengthBar password={pw} />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="su-pw2">パスワード (確認)</Label>
            <PasswordInput id="su-pw2" autoComplete="new-password" value={pw2} onChange={(e) => setPw2(e.target.value)} aria-invalid={mismatch} />
            {mismatch && <p className="text-xs text-red-500">パスワードが一致しません</p>}
          </div>
          <p className="text-xs text-muted-foreground">長くて推測されにくい文字列 (パスフレーズ) を推奨します。パスワードマネージャーでの保管をおすすめします。</p>
          {error && <p className="text-sm text-red-500">{error}</p>}
          <Button type="submit" disabled={!valid || pending}>
            {pending && <Loader2Icon className="animate-spin" />} アカウントを作成して開始
          </Button>
        </form>
      </CardContent>
    </Card>
  )
}

function LoginCard({ onDone }: { onDone: () => Promise<void> }) {
  const [username, setUsername] = useState("")
  const [pw, setPw] = useState("")
  const [totp, setTotp] = useState("")
  const [needTotp, setNeedTotp] = useState(false)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (pending) return
    setPending(true)
    setError(null)
    try {
      await api("/auth/login", { method: "POST", json: { username: username.trim(), password: pw, ...(needTotp ? { totp: totp.trim() } : {}) } })
      setPw("")
      setTotp("")
      await onDone()
    } catch (err) {
      if (err instanceof ApiError && err.status === 401 && err.totpRequired) {
        setNeedTotp(true)
        setError(totp ? "認証コードが正しくありません" : null)
      } else {
        // 429 (rate limited) and wrong credentials both surface the server's message.
        setError((err as Error).message)
      }
    } finally {
      setPending(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <LockKeyholeIcon className="size-4" /> ログイン
        </CardTitle>
        <CardDescription>{needTotp ? "認証アプリに表示されている 6 桁のコードを入力してください" : "LLM Harness にサインインします"}</CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="grid gap-4">
          <div className="grid gap-1.5">
            <Label htmlFor="li-user">ユーザー名</Label>
            <Input id="li-user" autoComplete="username" autoFocus value={username} onChange={(e) => setUsername(e.target.value)} />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="li-pw">パスワード</Label>
            <PasswordInput id="li-pw" autoComplete="current-password" value={pw} onChange={(e) => setPw(e.target.value)} />
          </div>
          {needTotp && (
            <div className="grid gap-1.5">
              <Label htmlFor="li-totp">認証コード</Label>
              <Input
                id="li-totp"
                inputMode="numeric"
                autoComplete="one-time-code"
                placeholder="123456"
                maxLength={8}
                autoFocus
                value={totp}
                onChange={(e) => setTotp(e.target.value.replace(/\s/g, ""))}
                className="text-center font-mono text-lg tracking-widest"
              />
            </div>
          )}
          {error && <p className="text-sm text-red-500">{error}</p>}
          <Button type="submit" disabled={pending || !username.trim() || !pw || (needTotp && totp.length < 6)}>
            {pending && <Loader2Icon className="animate-spin" />} ログイン
          </Button>
        </form>
      </CardContent>
    </Card>
  )
}
