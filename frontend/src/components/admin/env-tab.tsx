import { useEffect, useState } from "react"
import { PlusIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { ErrorAlert, LoadingLines, SaveBar } from "@/components/admin/shared"
import { isMasked, isSecretName } from "@/components/admin/util"
import type { EnvResponse } from "@/components/admin/types"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { PasswordInput } from "@/components/password-input"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { api, usePoll } from "@/lib/api"

const MAX_ENTRIES = 200
const DEFAULT_PATTERN = "[A-Z_][A-Z0-9_]*"

type Row = { uid: number; name: string; value: string; orig: string | null; masked: boolean }

let uidSeq = 0
const nextUid = () => ++uidSeq

function rowsFrom(env: Record<string, string>): Row[] {
  return Object.entries(env).map(([name, value]) => ({ uid: nextUid(), name, value, orig: name, masked: isMasked(value) }))
}

function rowError(r: Row, idx: number, all: Row[], pattern: RegExp, deny: string[]): string | undefined {
  if (!r.name) return "名前を入力してください"
  if (!pattern.test(r.name)) return "大文字・数字・_ のみ使えます (先頭は数字以外)"
  if (deny.includes(r.name)) return "禁止された名前です"
  if (all.findIndex((x) => x.name === r.name) !== idx) return "名前が重複しています"
  if (r.masked && r.name !== r.orig) return "マスクされた値は別の名前では使えません。値を入力し直してください"
  if (r.value.length > 8192 || r.value.includes("\0")) return "値が不正です (8192 文字以内)"
  return undefined
}

export function EnvTab() {
  const { data, error, refetch } = usePoll<EnvResponse>("/admin/env", 0)
  const [rows, setRows] = useState<Row[]>([])
  const [saveError, setSaveError] = useState<string | null>(null)
  const [confirmOpen, setConfirmOpen] = useState(false)

  useEffect(() => {
    if (data) setRows(rowsFrom(data.env))
  }, [data])

  const env = data?.env ?? {}
  const deny = data?.deny ?? []
  const pattern = new RegExp(`^(?:${data?.name_pattern ?? DEFAULT_PATTERN})$`)

  const errors = rows.map((r, i) => rowError(r, i, rows, pattern, deny))
  const countError = rows.length > MAX_ENTRIES ? `${MAX_ENTRIES} 件までです` : undefined
  const hasError = errors.some(Boolean) || countError !== undefined

  const added = rows.filter((r) => r.orig === null).map((r) => r.name)
  const changed = rows
    .filter((r) => r.orig !== null && (r.name !== r.orig || r.value !== env[r.orig]))
    .map((r) => (r.name !== r.orig ? `${r.orig} → ${r.name}` : r.name))
  const removed = Object.keys(env).filter((k) => !rows.some((r) => r.orig === k))
  const dirtyCount = added.length + changed.length + removed.length

  function update(uid: number, patch: Partial<Row>) {
    setRows((rs) => rs.map((r) => (r.uid === uid ? { ...r, ...patch } : r)))
  }

  async function save() {
    setSaveError(null)
    const payload = Object.fromEntries(rows.map((r) => [r.name, r.value]))
    try {
      await api<unknown>("/admin/env", { method: "PUT", json: { env: payload } })
      toast.success("環境変数を保存しました", { description: "新規に起動するモデルサーバから反映されます" })
      await refetch()
    } catch (e) {
      setSaveError((e as Error).message)
      throw e
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>環境変数</CardTitle>
        <CardDescription>
          モデルサーバ起動時に追加する環境変数。保存すると全体が置き換わります (最大 {MAX_ENTRIES} 件)。
        </CardDescription>
        {data && (
          <p className="text-xs text-muted-foreground break-words">
            名前の形式: <code className="font-mono">{data.name_pattern}</code> · 禁止: <code className="font-mono">{data.deny.join(", ")}</code>
          </p>
        )}
      </CardHeader>
      <CardContent className="grid gap-4">
        {error && <ErrorAlert error={error} />}
        {saveError && <ErrorAlert error={saveError} title="保存できませんでした" />}

        {!data ? (
          !error && <LoadingLines rows={4} />
        ) : rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">環境変数はありません。</p>
        ) : (
          <div className="grid gap-3">
            {rows.map((r, i) => {
              const err = errors[i]
              return (
                <div key={r.uid} className="grid gap-2 rounded-md border p-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.6fr)_auto] sm:items-start sm:border-0 sm:p-0">
                  <div className="grid gap-1">
                    <Input
                      aria-label="環境変数名"
                      placeholder="NAME"
                      className="font-mono text-sm"
                      value={r.name}
                      aria-invalid={err !== undefined}
                      onChange={(e) => update(r.uid, { name: e.target.value.toUpperCase() })}
                    />
                    {err && <p className="text-xs text-destructive">{err}</p>}
                  </div>
                  {isSecretName(r.name) ? (
                    <PasswordInput
                      aria-label="値"
                      className="font-mono text-sm"
                      value={r.value}
                      onChange={(e) => update(r.uid, { value: e.target.value, masked: false })}
                    />
                  ) : (
                    <Input
                      aria-label="値"
                      className="font-mono text-sm"
                      value={r.value}
                      onChange={(e) => update(r.uid, { value: e.target.value, masked: false })}
                    />
                  )}
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label="この行を削除"
                    title="削除"
                    onClick={() => setRows((rs) => rs.filter((x) => x.uid !== r.uid))}
                  >
                    <Trash2Icon />
                  </Button>
                </div>
              )
            })}
          </div>
        )}

        {countError && <p className="text-xs text-destructive">{countError}</p>}
        {data && (
          <div className="flex flex-wrap items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              disabled={rows.length >= MAX_ENTRIES}
              onClick={() => setRows((rs) => [...rs, { uid: nextUid(), name: "", value: "", orig: null, masked: false }])}
            >
              <PlusIcon /> 行を追加
            </Button>
            <span className="text-xs text-muted-foreground tabular-nums">
              {rows.length} / {MAX_ENTRIES} 件
            </span>
          </div>
        )}

        <SaveBar
          count={dirtyCount}
          saving={false}
          canSave={!hasError}
          onSave={() => setConfirmOpen(true)}
          onDiscard={() => data && setRows(rowsFrom(data.env))}
        />
      </CardContent>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="環境変数を保存"
        description="全体を置き換えます。新しく起動するモデルサーバから反映されます。値は表示しません。"
        confirmLabel="保存"
        onConfirm={save}
      >
        <ul className="grid gap-1 text-sm">
          {added.length > 0 && <li>追加: <span className="font-mono break-all">{added.join(", ")}</span></li>}
          {changed.length > 0 && <li>変更: <span className="font-mono break-all">{changed.join(", ")}</span></li>}
          {removed.length > 0 && <li>削除: <span className="font-mono break-all">{removed.join(", ")}</span></li>}
        </ul>
      </ConfirmDialog>
    </Card>
  )
}
