import { useState } from "react"
import { PlayIcon, PowerIcon, PowerOffIcon, RocketIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { LaunchDialog } from "@/components/launch-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, usePoll, type LocalModel, type Profile } from "@/lib/api"
import { bytes } from "@/lib/format"

type OllamaModel = { name: string; size: number; details?: { parameter_size?: string; quantization_level?: string; family?: string } }
type OllamaState = { available: boolean; models: OllamaModel[]; running: { name: string; size_vram: number }[] }

export function ModelsPage() {
  const { data: models, refetch } = usePoll<LocalModel[]>("/models", 10000)
  const { data: ollama, refetch: refetchOllama } = usePoll<OllamaState>("/ollama/models", 5000)
  const [launch, setLaunch] = useState<Partial<Profile> | null>(null)

  async function remove(m: LocalModel) {
    if (!confirm(`${m.id} を削除しますか? (${bytes(m.size)})`)) return
    await api(`/models/${m.id}`, { method: "DELETE" })
    toast.success("削除しました")
    refetch()
  }

  async function ollamaAction(model: string, action: "load" | "unload" | "delete") {
    if (action === "delete" && !confirm(`${model} を削除しますか?`)) return
    const t = toast.loading(`${model}: ${action}…`)
    try {
      await api(`/ollama/${action}`, { method: "POST", json: { model } })
      toast.success(`${model}: ${action} 完了`, { id: t })
    } catch (e) {
      toast.error((e as Error).message, { id: t })
    }
    refetchOllama()
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
          <Table>
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
                      <Button
                        size="icon-sm"
                        variant="ghost"
                        title="起動"
                        onClick={() => setLaunch({ runtime: m.format === "gguf" ? "llama.cpp" : "vllm", model: m.id })}
                      >
                        <RocketIcon />
                      </Button>
                      <Button size="icon-sm" variant="ghost" title="削除" onClick={() => remove(m)}>
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
                        <div className="flex gap-1">
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
                            <Button size="icon-sm" variant="ghost" title="アンロード" onClick={() => ollamaAction(m.name, "unload")}>
                              <PowerOffIcon />
                            </Button>
                          ) : (
                            <Button size="icon-sm" variant="ghost" title="VRAM にロード" onClick={() => ollamaAction(m.name, "load")}>
                              <PowerIcon />
                            </Button>
                          )}
                          <Button size="icon-sm" variant="ghost" title="削除" onClick={() => ollamaAction(m.name, "delete")}>
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

      <LaunchDialog open={!!launch} onOpenChange={(o) => !o && setLaunch(null)} initial={launch ?? undefined} />
    </div>
  )
}
