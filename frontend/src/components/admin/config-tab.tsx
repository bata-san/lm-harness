import { useEffect, useRef, useState } from "react"
import { DownloadIcon, UploadIcon, XIcon } from "lucide-react"
import { toast } from "sonner"

import { ErrorAlert, LoadingLines, SaveBar } from "@/components/admin/shared"
import { downloadJson } from "@/components/admin/util"
import { ConfigHistory } from "@/components/admin/config-history"
import type { AdminConfig, ConfigSaveResult, SchemaEntry } from "@/components/admin/types"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { PasswordInput } from "@/components/password-input"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"
import { api, usePoll } from "@/lib/api"
import { dateTime } from "@/lib/format"
import { cn } from "@/lib/utils"

const LABELS: Record<string, string> = {
  models_dir: "モデル置き場",
  hf_token: "Hugging Face トークン",
  hf_endpoint: "HF エンドポイント",
  llama_cpp_dir: "llama.cpp ディレクトリ",
  vllm_venv: "vLLM venv",
  strata_dir: "Strata ディレクトリ",
  ollama_url: "Ollama URL",
  max_concurrent_downloads: "同時ダウンロード数",
  connections_per_file: "1 ファイルあたりの接続数",
  default_host: "バインドアドレス",
  port_range_start: "ポート自動割当の開始番号",
  extra_env: "追加環境変数",
  mode: "性能モード設定",
}

type Kind = "text" | "secret" | "number" | "boolean" | "json" | "readonly"
type Draft = Record<string, unknown>
type Confirm = { kind: "rollback"; version: string } | { kind: "import"; name: string; doc: Record<string, unknown> }

/** Schema `type` decides the control; the current value is only a fallback. */
function kindOf(entry: SchemaEntry | undefined, value: unknown): Kind {
  const t = entry?.type ?? ""
  if (t === "secret") return "secret"
  if (t === "env-map") return "readonly"
  if (t === "object") return "json"
  if (t === "boolean" || typeof value === "boolean") return "boolean"
  if (/^(integer|port)/.test(t) || typeof value === "number") return "number"
  return "text"
}

function toDraft(settings: Record<string, unknown>, schema: Record<string, SchemaEntry>): Draft {
  const d: Draft = {}
  for (const key of Object.keys(schema)) {
    if (!(key in settings)) continue
    const v = settings[key]
    const kind = kindOf(schema[key], v)
    d[key] = kind === "json" ? JSON.stringify(v, null, 2) : kind === "number" ? String(v ?? "") : kind === "readonly" ? v : (v ?? "")
  }
  return d
}

/** Turns one edited draft value into the JSON value sent to the server (or a client-side error). */
function buildValue(raw: unknown, kind: Kind, entry: SchemaEntry): { value?: unknown; error?: string } {
  switch (kind) {
    case "text":
    case "secret":
      return { value: String(raw ?? "") }
    case "boolean":
      return { value: Boolean(raw) }
    case "number": {
      const s = String(raw ?? "").trim()
      const n = Number(s)
      if (s === "" || !Number.isInteger(n)) return { error: "整数で入力してください" }
      const range = entry.type.match(/\((\d+)-(\d+)\)/)
      if (range && (n < Number(range[1]) || n > Number(range[2]))) return { error: `${range[1]}〜${range[2]} の範囲で入力してください` }
      return { value: n }
    }
    case "json": {
      try {
        const v = JSON.parse(String(raw))
        if (v === null || typeof v !== "object" || Array.isArray(v)) return { error: "JSON オブジェクトで入力してください" }
        return { value: v }
      } catch {
        return { error: "JSON の構文が不正です" }
      }
    }
    case "readonly":
      return {}
  }
}

function formatVersion(v: string): string {
  return `${v.slice(0, 4)}-${v.slice(4, 6)}-${v.slice(6, 8)} ${v.slice(9, 11)}:${v.slice(11, 13)}:${v.slice(13, 15)}`
}

export function ConfigTab() {
  const { data, error, refetch } = usePoll<AdminConfig>("/admin/config", 0)
  const [draft, setDraft] = useState<Draft>({})
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [notice, setNotice] = useState<ConfigSaveResult | null>(null)
  const [confirm, setConfirm] = useState<Confirm | null>(null)
  const [exporting, setExporting] = useState(false)
  const fileRef = useRef<HTMLInputElement>(null)

  // Whenever the server state is (re)loaded, the form starts from it again.
  useEffect(() => {
    if (data) setDraft(toDraft(data.settings, data.schema))
  }, [data])

  const base = data ? toDraft(data.settings, data.schema) : {}
  const dirtyKeys = Object.keys(draft).filter((k) => draft[k] !== base[k])

  // Validate dirty keys; only the valid ones go into the PUT body.
  const patch: Record<string, unknown> = {}
  const fieldErrors: Record<string, string> = {}
  for (const key of dirtyKeys) {
    const entry = data?.schema[key]
    if (!data || !entry) continue
    const kind = kindOf(entry, data.settings[key])
    if (kind === "readonly") continue
    const r = buildValue(draft[key], kind, entry)
    if (r.error) fieldErrors[key] = r.error
    else patch[key] = r.value
  }
  const canSave = dirtyKeys.length > 0 && Object.keys(fieldErrors).length === 0

  function set(key: string, value: unknown) {
    setDraft((d) => ({ ...d, [key]: value }))
  }

  async function save() {
    if (!canSave || saving) return
    setSaving(true)
    setSaveError(null)
    try {
      const r = await api<ConfigSaveResult>("/admin/config", { method: "PUT", json: patch })
      setNotice(r)
      toast.success(r.changed.length ? "保存しました" : "変更はありません", {
        description: r.restart_required.length ? `再起動が必要: ${r.restart_required.join(", ")}` : undefined,
      })
      await refetch()
    } catch (e) {
      setSaveError((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  async function exportConfig() {
    setExporting(true)
    try {
      const doc = await api<unknown>("/admin/config/export")
      downloadJson("llm-harness-config.json", doc)
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setExporting(false)
    }
  }

  async function onPickFile(file: File | undefined) {
    if (!file) return
    let doc: unknown
    try {
      doc = JSON.parse(await file.text())
    } catch {
      toast.error("JSON として読み込めません")
      return
    }
    if (!doc || typeof doc !== "object" || Array.isArray(doc)) {
      toast.error("エクスポートした設定 JSON ではありません")
      return
    }
    setConfirm({ kind: "import", name: file.name, doc: doc as Record<string, unknown> })
  }

  async function runConfirm(c: Confirm) {
    const r =
      c.kind === "rollback"
        ? await api<ConfigSaveResult>("/admin/config/rollback", { method: "POST", json: { version: c.version } })
        : await api<ConfigSaveResult>("/admin/config/import", { method: "POST", json: c.doc })
    setNotice(r)
    setSaveError(null)
    toast.success(c.kind === "rollback" ? "ロールバックしました" : "インポートしました", {
      description: r.restart_required.length ? `再起動が必要: ${r.restart_required.join(", ")}` : undefined,
    })
    await refetch()
  }

  return (
    <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_20rem]">
      <Card className="min-w-0">
        <CardHeader>
          <CardTitle>設定</CardTitle>
          <CardDescription>~/.llm-harness/settings.json。保存するたびに変更前の内容が履歴に残ります。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-5">
          <div className="flex flex-wrap gap-2">
            <Button variant="outline" size="sm" disabled={exporting || !data} onClick={exportConfig}>
              <DownloadIcon /> エクスポート
            </Button>
            <Button variant="outline" size="sm" disabled={!data} onClick={() => fileRef.current?.click()}>
              <UploadIcon /> インポート
            </Button>
            <input
              ref={fileRef}
              type="file"
              accept="application/json,.json"
              className="hidden"
              onChange={(e) => {
                const f = e.target.files?.[0]
                e.target.value = ""
                void onPickFile(f)
              }}
            />
          </div>

          {error && <ErrorAlert error={error} />}
          {saveError && <ErrorAlert error={saveError} title="保存できませんでした" />}
          {notice && (notice.changed.length > 0 || notice.restart_required.length > 0) && (
            <Alert>
              <AlertTitle className="flex items-center justify-between">
                直前の変更
                <Button variant="ghost" size="icon-sm" aria-label="閉じる" onClick={() => setNotice(null)}>
                  <XIcon />
                </Button>
              </AlertTitle>
              <AlertDescription className="grid gap-2">
                {notice.changed.length > 0 && <span className="break-all">変更: {notice.changed.join(", ")}</span>}
                {notice.restart_required.length > 0 ? (
                  <span className="flex flex-wrap items-center gap-1.5">
                    再起動が必要:
                    {notice.restart_required.map((k) => (
                      <Badge key={k} variant="outline" className="border-amber-500 text-amber-700 dark:text-amber-400">
                        {k}
                      </Badge>
                    ))}
                  </span>
                ) : (
                  notice.changed.length > 0 && <span>すべて即時に反映されます</span>
                )}
              </AlertDescription>
            </Alert>
          )}

          {!data ? (
            !error && <LoadingLines rows={8} />
          ) : (
            <div className="grid gap-5">
              {Object.keys(data.schema)
                .filter((key) => key in data.settings)
                .map((key) => {
                  const entry = data.schema[key]
                  const kind = kindOf(entry, data.settings[key])
                  const id = `cfg-${key}`
                  const dirty = draft[key] !== undefined && draft[key] !== base[key]
                  const value = draft[key]
                  return (
                    <div key={key} className={cn("grid gap-1.5 rounded-md p-2 -mx-2", dirty && "bg-amber-500/5")}>
                      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                        <Label htmlFor={id}>{LABELS[key] ?? key}</Label>
                        <code className="text-[11px] text-muted-foreground">{key}</code>
                        {entry.restart_required && (
                          <Badge variant="outline" className="border-amber-500 text-amber-700 dark:text-amber-400">
                            再起動が必要
                          </Badge>
                        )}
                        {dirty && <Badge>変更あり</Badge>}
                      </div>

                      {kind === "text" && (
                        <Input id={id} className="font-mono text-sm" value={String(value ?? "")} onChange={(e) => set(key, e.target.value)} />
                      )}
                      {kind === "number" && (
                        <Input id={id} type="number" inputMode="numeric" className="max-w-48" value={String(value ?? "")} onChange={(e) => set(key, e.target.value)} />
                      )}
                      {kind === "secret" && (
                        <PasswordInput id={id} className="font-mono text-sm" value={String(value ?? "")} onChange={(e) => set(key, e.target.value)} />
                      )}
                      {kind === "boolean" && (
                        <Switch id={id} checked={Boolean(value)} onCheckedChange={(c) => set(key, c)} />
                      )}
                      {kind === "json" && (
                        <Textarea id={id} rows={10} spellCheck={false} className="font-mono text-xs" value={String(value ?? "")} onChange={(e) => set(key, e.target.value)} />
                      )}
                      {kind === "readonly" && (
                        <>
                          <pre className="max-h-48 overflow-auto rounded-md border bg-muted/40 p-2 font-mono text-xs">
                            {JSON.stringify(data.settings[key], null, 2)}
                          </pre>
                          <p className="text-xs text-muted-foreground">秘密の値はマスクされています。編集は「環境変数」タブで行います。</p>
                        </>
                      )}

                      <p className="text-xs text-muted-foreground">{entry.description}</p>
                      {kind === "secret" && (
                        <p className="text-xs text-muted-foreground">マスク表示のまま保存すると変更されません。空欄で保存すると削除されます。</p>
                      )}
                      {fieldErrors[key] && <p className="text-xs text-destructive">{fieldErrors[key]}</p>}
                    </div>
                  )
                })}
            </div>
          )}

          <SaveBar
            count={dirtyKeys.length}
            saving={saving}
            canSave={canSave}
            onSave={save}
            onDiscard={() => data && setDraft(toDraft(data.settings, data.schema))}
          >
            {Object.keys(fieldErrors).length > 0 && (
              <p className="text-xs text-destructive">入力内容を確認してください ({Object.keys(fieldErrors).join(", ")})</p>
            )}
          </SaveBar>
        </CardContent>
      </Card>

      <Card className="h-fit min-w-0">
        <CardHeader>
          <CardTitle>変更履歴</CardTitle>
          <CardDescription>直近 20 件。ロールバックすると、その時点の設定に戻します。</CardDescription>
        </CardHeader>
        <CardContent>
          {data ? (
            <ConfigHistory
              history={data.history}
              onRollback={(version) => setConfirm({ kind: "rollback", version })}
              format={(h) => (h.created_at ? dateTime(h.created_at) : formatVersion(h.version))}
            />
          ) : (
            !error && <LoadingLines rows={4} />
          )}
        </CardContent>
      </Card>

      <ConfirmDialog
        open={confirm !== null}
        onOpenChange={(o) => !o && setConfirm(null)}
        title={confirm?.kind === "import" ? "設定をインポート" : "設定をロールバック"}
        description={
          confirm?.kind === "import"
            ? "取り込んだ内容で現在の設定を上書きします。秘密の値 (HF トークン・秘密の環境変数) は取り込まれず、変更前の設定は履歴に残ります。"
            : "選んだ時点の設定に戻します。現在の設定は新しい履歴として残ります。"
        }
        confirmLabel={confirm?.kind === "import" ? "インポート" : "ロールバック"}
        onConfirm={() => (confirm ? runConfirm(confirm) : undefined)}
      >
        {confirm?.kind === "import" && (
          <p className="break-all rounded-md border bg-muted/40 p-2 font-mono text-xs">{confirm.name}</p>
        )}
        {confirm?.kind === "rollback" && (
          <p className="rounded-md border bg-muted/40 p-2 font-mono text-xs break-all">{formatVersion(confirm.version)}</p>
        )}
        {dirtyKeys.length > 0 && <p className="text-sm text-amber-700 dark:text-amber-400">未保存の変更は破棄されます。</p>}
      </ConfirmDialog>
    </div>
  )
}
