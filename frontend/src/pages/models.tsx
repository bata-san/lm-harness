import { useState } from "react"
import { PlayIcon, PowerIcon, PowerOffIcon, RocketIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { LaunchDialog } from "@/components/launch-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, usePoll, type Instance, type LocalModel, type Profile, type UninstallResult } from "@/lib/api"
import { bytes } from "@/lib/format"

type OllamaModel = { name: string; size: number; details?: { parameter_size?: string; quantization_level?: string; family?: string } }
type OllamaState = { available: boolean; models: OllamaModel[]; running: { name: string; size_vram: number }[] }

export function ModelsPage() {
  const { data: models, refetch } = usePoll<LocalModel[]>("/models", 10000)
  const { data: ollama, refetch: refetchOllama } = usePoll<OllamaState>("/ollama/models", 5000)
  const [launch, setLaunch] = useState<Partial<Profile> | null>(null)

  const { data: instances } = usePoll<Instance[]>("/instances", 5000)
  const { data: profiles } = usePoll<Profile[]>("/profiles", 0)
  const [uninstall, setUninstall] = useState<LocalModel | null>(null)
  const [ollamaDel, setOllamaDel] = useState<string | null>(null)
  const [ollamaBusy, setOllamaBusy] = useState<string | null>(null)

  async function ollamaAction(model: string, action: "load" | "unload" | "delete") {
    if (ollamaBusy) return
    setOllamaBusy(model)
    const t = toast.loading(`${model}: ${action}…`)
    try {
      await api(`/ollama/${action}`, { method: "POST", json: { model } })
      toast.success(`${model}: ${action} 完了`, { id: t })
    } catch (e) {
      toast.error((e as Error).message, { id: t })
    } finally {
      setOllamaBusy(null)
    }
    refetchOllama()
  }

  // Best-effort preview of what an uninstall will touch (the backend does the authoritative cleanup).
  const uses = (model: string, id: string) => model === id || model.includes(id)
  const affectedServers = uninstall ? (instances ?? []).filter((i) => (i.status === "running" || i.status === "starting") && uses(i.model, uninstall.id)) : []
  const affectedProfiles = uninstall ? (profiles ?? []).filter((p) => uses(p.model, uninstall.id)) : []

  function launchButton(m: LocalModel) {
    return (
      <Button size="icon-sm" variant="ghost" title="起動" aria-label="起動" onClick={() => setLaunch({ runtime: m.format === "gguf" ? "llama.cpp" : "vllm", model: m.id })}>
        <RocketIcon />
      </Button>
    )
  }

  const totalSize = (models ?? []).reduce((s, m) => s + m.size, 0)

  return (
    <div className="grid gap-4">
      <Card>
        <CardHeader>
          <CardTitle>ローカルモデル</CardTitle>
          <CardDescription>
            {(models ?? []).length} モデル · 合計 {bytes(totalSize)} · GGUF メタデータを直接解析
          </CardDescription>
        </CardHeader>
        <CardContent>
          <div className="grid gap-2 md:hidden">
            {(models ?? []).map((m) => (
              <div key={m.id} className="grid gap-2 rounded-lg border p-3">
                <div className="min-w-0">
                  <div className="font-medium break-all">{m.info.name || m.name}</div>
                  <div className="font-mono text-xs break-all text-muted-foreground">{m.id}</div>
                </div>
                <div className="flex flex-wrap items-center gap-1 text-xs">
                  <Badge variant="outline">{m.format === "gguf" ? m.info.architecture ?? "gguf" : "safetensors"}</Badge>
                  {m.info.file_type && <Badge variant="secondary">{m.info.file_type}</Badge>}
                  {m.info.expert_count ? (
                    <Badge variant="secondary">
                      MoE {m.info.expert_used_count}/{m.info.expert_count}
                    </Badge>
                  ) : null}
                  {m.mmproj && <Badge variant="secondary">vision</Badge>}
                  <span className="ml-auto tabular-nums">{bytes(m.size)}</span>
                </div>
                <div className="flex gap-2">
                  <Button variant="outline" className="flex-1" onClick={() => setLaunch({ runtime: m.format === "gguf" ? "llama.cpp" : "vllm", model: m.id })}>
                    <RocketIcon /> 起動
                  </Button>
                  <Button variant="destructive" className="flex-1" onClick={() => setUninstall(m)}>
                    <Trash2Icon /> アンインストール
                  </Button>
                </div>
              </div>
            ))}
            {models?.length === 0 && <p className="text-sm text-muted-foreground">モデルがありません。「ダウンロード」から取得してください。</p>}
          </div>
          <Table className="hidden md:table">
            <TableHeader>
              <TableRow>
                <TableHead>モデル</TableHead>
                <TableHead>アーキテクチャ</TableHead>
                <TableHead>量子化</TableHead>
                <TableHead className="text-right">Context</TableHead>
                <TableHead className="text-right">サイズ</TableHead>
                <TableHead className="w-24" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {(models ?? []).map((m) => (
                <TableRow key={m.id}>
                  <TableCell className="max-w-md">
                    <div className="truncate font-medium">{m.info.name || m.name}</div>
                    <div className="truncate font-mono text-xs text-muted-foreground">{m.id}</div>
                  </TableCell>
                  <TableCell>
                    <div className="flex flex-wrap gap-1">
                      <Badge variant="outline">{m.format === "gguf" ? m.info.architecture ?? "gguf" : "safetensors"}</Badge>
                      {m.info.size_label && <Badge variant="secondary">{m.info.size_label}</Badge>}
                      {m.info.expert_count ? (
                        <Badge variant="secondary">
                          MoE {m.info.expert_used_count}/{m.info.expert_count}
                        </Badge>
                      ) : null}
                      {m.mmproj && <Badge variant="secondary">vision</Badge>}
                      {m.files > 1 && <Badge variant="secondary">{m.files} parts</Badge>}
                    </div>
                  </TableCell>
                  <TableCell>{m.info.file_type ?? "—"}</TableCell>
                  <TableCell className="text-right tabular-nums">{m.info.context_length?.toLocaleString() ?? "—"}</TableCell>
                  <TableCell className="text-right tabular-nums">{bytes(m.size)}</TableCell>
                  <TableCell>
                    <div className="flex justify-end gap-1">
                      {launchButton(m)}
                      <Button size="icon-sm" variant="ghost" title="アンインストール" aria-label="アンインストール" onClick={() => setUninstall(m)}>
                        <Trash2Icon />
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
              {models?.length === 0 && (
                <TableRow>
                  <TableCell colSpan={6} className="text-muted-foreground">
                    モデルがありません。「ダウンロード」から取得してください。
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Ollama モデル</CardTitle>
          <CardDescription>{ollama?.available ? `${ollama.models.length} モデル / ${ollama.running.length} ロード中` : "Ollama に接続できません (未インストール or 停止中)"}</CardDescription>
        </CardHeader>
        {ollama?.available && (
          <CardContent>
            <Table>
              <TableBody>
                {ollama.models.map((m) => {
                  const loaded = ollama.running.find((r) => r.name === m.name)
                  return (
                    <TableRow key={m.name}>
                      <TableCell className="font-medium">{m.name}</TableCell>
                      <TableCell>
                        <div className="flex flex-wrap gap-1">
                          {m.details?.parameter_size && <Badge variant="secondary">{m.details.parameter_size}</Badge>}
                          {m.details?.quantization_level && <Badge variant="outline">{m.details.quantization_level}</Badge>}
                          {loaded && (
                            <Badge className="bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
                              <PlayIcon /> VRAM {bytes(loaded.size_vram)}
                            </Badge>
                          )}
                        </div>
                      </TableCell>
                      <TableCell className="text-right tabular-nums">{bytes(m.size)}</TableCell>
                      <TableCell>
                        <div className="flex justify-end gap-1">
                          {loaded ? (
                            <Button size="icon-sm" variant="ghost" title="アンロード" disabled={ollamaBusy === m.name} onClick={() => ollamaAction(m.name, "unload")}>
                              <PowerOffIcon />
                            </Button>
                          ) : (
                            <Button size="icon-sm" variant="ghost" title="VRAM にロード" disabled={ollamaBusy === m.name} onClick={() => ollamaAction(m.name, "load")}>
                              <PowerIcon />
                            </Button>
                          )}
                          <Button size="icon-sm" variant="ghost" title="削除" disabled={ollamaBusy === m.name} onClick={() => setOllamaDel(m.name)}>
                            <Trash2Icon />
                          </Button>
                        </div>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          </CardContent>
        )}
      </Card>

      <ConfirmDialog
        open={!!uninstall}
        onOpenChange={(o) => !o && setUninstall(null)}
        title="モデルをアンインストールしますか?"
        description={uninstall ? <span className="font-mono break-all">{uninstall.id}</span> : undefined}
        confirmLabel="アンインストール"
        onConfirm={async () => {
          if (!uninstall) return
          const r = await api<UninstallResult>("/catalog/uninstall", { method: "POST", json: { model: uninstall.id } })
          toast.success("アンインストールしました", {
            description: `${bytes(r.freed_bytes)} を解放 · サーバ ${r.stopped.length} 台を停止 · プロファイル ${r.profiles_removed} 件を削除`,
          })
          refetch()
        }}
      >
        {uninstall && (
          <ul className="grid gap-1.5 rounded-lg border bg-muted/40 p-3 text-sm">
            <li>
              <b className="tabular-nums">{bytes(uninstall.size)}</b> のファイルを削除します (元に戻せません)
            </li>
            <li>
              このモデルを使っている稼働中のサーバは<b>停止</b>されます
              {affectedServers.length > 0 && <span className="text-red-500"> (現在 {affectedServers.map((i) => i.name).join(", ")})</span>}
            </li>
            <li>
              このモデルの起動プロファイルは<b>削除</b>されます
              {affectedProfiles.length > 0 && <span className="text-muted-foreground"> ({affectedProfiles.map((p) => p.name).join(", ")})</span>}
            </li>
          </ul>
        )}
      </ConfirmDialog>

      <ConfirmDialog
        open={!!ollamaDel}
        onOpenChange={(o) => !o && setOllamaDel(null)}
        title="Ollama モデルを削除しますか?"
        description={ollamaDel ?? undefined}
        confirmLabel="削除する"
        onConfirm={async () => {
          if (ollamaDel) await ollamaAction(ollamaDel, "delete")
        }}
      />

      <LaunchDialog open={!!launch} onOpenChange={(o) => !o && setLaunch(null)} initial={launch ?? undefined} />
    </div>
  )
}
