import { useCallback, useEffect, useReducer, useState, type ReactNode } from "react"
import { Loader2Icon, LockKeyholeIcon, RefreshCwIcon, ShieldAlertIcon } from "lucide-react"

import { PasswordInput } from "@/components/password-input"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import { VaultContext } from "@/components/chat/vault-context"
import { readIdlePref } from "@/lib/vault-prefs"
import { cn } from "@/lib/utils"
import { getMeta, initVault, passphraseStrength, VaultError, VaultSession, type VaultMeta } from "@/lib/vault"

const MIN_PASSPHRASE_CHARS = 8

/**
 * Owns the single VaultSession of the chat page. Shows first-time setup, the unlock form,
 * or (when unlocked) the children with the session in context. The key never leaves memory:
 * locking (button or idle timer) unmounts the children and drops the key.
 */
export function VaultGate({ children }: { children: ReactNode }) {
  const [, bump] = useReducer((n: number) => n + 1, 0)
  const [session] = useState(() => new VaultSession({ idleMinutes: readIdlePref(), onLock: () => bump() }))
  const [meta, setMeta] = useState<VaultMeta | undefined>()
  const [loadError, setLoadError] = useState<string | undefined>()
  const unlocked = session.isUnlocked

  const load = useCallback(async () => {
    try {
      setMeta(await getMeta())
      setLoadError(undefined)
    } catch (e) {
      setLoadError((e as Error).message)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  // Any user activity postpones the idle auto-lock.
  useEffect(() => {
    if (!unlocked) return
    const events = ["pointerdown", "keydown", "wheel", "touchstart"] as const
    const on = () => session.touch()
    for (const e of events) addEventListener(e, on, { passive: true })
    return () => {
      for (const e of events) removeEventListener(e, on)
    }
  }, [unlocked, session])

  if (unlocked) return <VaultContext.Provider value={session}>{children}</VaultContext.Provider>

  if (loadError) {
    return (
      <Alert variant="destructive" className="mx-auto max-w-md">
        <ShieldAlertIcon />
        <AlertTitle>保管庫の状態を取得できません</AlertTitle>
        <AlertDescription>
          <p>{loadError}</p>
          <Button size="sm" variant="outline" className="mt-2" onClick={() => void load()}>
            <RefreshCwIcon /> 再試行
          </Button>
        </AlertDescription>
      </Alert>
    )
  }
  if (!meta) {
    return (
      <div className="flex justify-center py-16 text-muted-foreground">
        <Loader2Icon className="animate-spin" />
      </div>
    )
  }
  if (!meta.salt) {
    return (
      <SetupCard
        session={session}
        onReady={() => {
          bump()
          void load()
        }}
      />
    )
  }
  return <UnlockCard session={session} meta={meta} onUnlocked={() => bump()} />
}

function GateCard({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return (
    <Card className="mx-auto w-full max-w-md">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <LockKeyholeIcon className="size-4" /> {title}
        </CardTitle>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      <CardContent>{children}</CardContent>
    </Card>
  )
}

function SetupCard({ session, onReady }: { session: VaultSession; onReady: () => void }) {
  const [pass, setPass] = useState("")
  const [pass2, setPass2] = useState("")
  const [ack, setAck] = useState(false)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const strength = passphraseStrength(pass)
  const tooShort = [...pass].length < MIN_PASSPHRASE_CHARS
  const mismatch = pass2.length > 0 && pass !== pass2
  const valid = !tooShort && pass === pass2 && ack && !pending
  const strengthColor = ["bg-red-500", "bg-amber-500", "bg-sky-500", "bg-emerald-500"][strength.score]

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!valid) return
    setPending(true)
    setError(null)
    try {
      await initVault(session, pass)
      onReady()
    } catch (err) {
      setError((err as Error).message)
      setPending(false)
    }
  }

  return (
    <GateCard title="履歴の暗号化パスフレーズを設定" description="チャット履歴はこの合言葉で暗号化されます。ログインパスワードとは別のものです。">
      <form onSubmit={submit} className="grid gap-4">
        <Alert variant="destructive">
          <ShieldAlertIcon />
          <AlertTitle>合言葉を忘れると履歴は復元できません</AlertTitle>
          <AlertDescription>
            <p>サーバには暗号文しか保存されません。合言葉をサーバに送ることも、再設定することもできません。</p>
          </AlertDescription>
        </Alert>

        <div className="grid gap-1.5">
          <Label htmlFor="vault-new">合言葉 (8 文字以上)</Label>
          <PasswordInput id="vault-new" autoComplete="new-password" value={pass} onChange={(e) => setPass(e.target.value)} disabled={pending} />
          {pass && (
            <div className="grid gap-1">
              <div className="flex gap-1">
                {[1, 2, 3].map((i) => (
                  <div key={i} className={cn("h-1 flex-1 rounded-full", strength.score >= i ? strengthColor : "bg-muted")} />
                ))}
              </div>
              <p className="text-xs text-muted-foreground">
                強さ: {strength.label}
                {tooShort && " (8 文字以上にしてください)"}
              </p>
            </div>
          )}
        </div>

        <div className="grid gap-1.5">
          <Label htmlFor="vault-confirm">合言葉 (確認)</Label>
          <PasswordInput id="vault-confirm" autoComplete="new-password" value={pass2} onChange={(e) => setPass2(e.target.value)} disabled={pending} />
          {mismatch && <p className="text-xs text-destructive">合言葉が一致しません</p>}
        </div>

        <label className="flex items-start gap-2 text-sm">
          <Checkbox checked={ack} onCheckedChange={(c) => setAck(c === true)} className="mt-0.5" />
          <span>合言葉を忘れると履歴が失われることを理解しました</span>
        </label>

        {error && <p className="text-sm text-destructive">{error}</p>}
        <Button type="submit" disabled={!valid}>
          {pending && <Loader2Icon className="animate-spin" />}
          {pending ? "暗号化の準備をしています…" : "保管庫を作成"}
        </Button>
      </form>
    </GateCard>
  )
}

function UnlockCard({ session, meta, onUnlocked }: { session: VaultSession; meta: VaultMeta; onUnlocked: () => void }) {
  const [pass, setPass] = useState("")
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!pass || pending) return
    setPending(true)
    setError(null)
    try {
      await session.unlock(pass, meta)
      onUnlocked()
    } catch (err) {
      setError(
        err instanceof VaultError && err.code === "auth-failed"
          ? "合言葉が違います"
          : `解除できません: ${(err as Error).message}`,
      )
      setPending(false)
    }
  }

  return (
    <GateCard title="履歴を解除" description="チャット履歴を表示するには合言葉を入力してください。">
      <form onSubmit={submit} className="grid gap-4">
        <div className="grid gap-1.5">
          <Label htmlFor="vault-pass">合言葉</Label>
          <PasswordInput
            id="vault-pass"
            autoComplete="off"
            autoFocus
            value={pass}
            onChange={(e) => setPass(e.target.value)}
            disabled={pending}
            aria-invalid={error ? true : undefined}
          />
          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>
        <Button type="submit" disabled={!pass || pending}>
          {pending && <Loader2Icon className="animate-spin" />}
          {pending ? "解除しています…" : "解除"}
        </Button>
        <p className="text-xs text-muted-foreground">鍵はこのタブのメモリにのみ保持され、ページを閉じると消えます。</p>
      </form>
    </GateCard>
  )
}
