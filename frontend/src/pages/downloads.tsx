import { useMemo, useState } from "react"
import { DownloadIcon, HeartIcon, PauseIcon, PlayIcon, SearchIcon, Trash2Icon, XIcon } from "lucide-react"
import { toast } from "sonner"

import { StatusBadge } from "@/components/status-badge"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Progress } from "@/components/ui/progress"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, usePoll, type Download, type HfFile, type HfRepo } from "@/lib/api"
import { bytes, duration, pct } from "@/lib/format"

export function DownloadsPage() {
  const { data: queue, refetch } = usePoll<Download[]>("/downloads", 1000)
  const [q, setQ] = useState("")
  const [ggufOnly, setGgufOnly] = useState(true)
  const [results, setResults] = useState<HfRepo[] | null>(null)
  const [searching, setSearching] = useState(false)
  const [repo, setRepo] = useState<string | null>(null)
  const [ollamaName, setOllamaName] = useState("")

  async function search(e?: React.FormEvent) {
    e?.preventDefault()
    if (!q.trim()) return
    // "owner/name" をそのまま貼った場合は直接ファイル一覧へ
    if (/^[\w.-]+\/[\w.-]+$/.test(q.trim()) && !q.includes(" ")) {
      setRepo(q.trim())
    }
    setSearching(true)
    try {
      setResults(await api<HfRepo[]>(`/hf/search?q=${encodeURIComponent(q)}&gguf=${ggufOnly}`))
    } catch (err) {
      toast.error((err as Error).message)
    } finally {
      setSearching(false)
    }
  }

  async function act(id: string, action: "pause" | "resume" | "cancel" | "delete") {
    try {
      if (action === "delete") await api(`/downloads/${id}`, { method: "DELETE" })
      else await api(`/downloads/${id}/${action}`, { method: "POST" })
      refetch()
    } catch (err) {
      toast.error((err as Error).message)
    }
  }

  async function pullOllama(e: React.FormEvent) {
    e.preventDefault()
    if (!ollamaName.trim()) return
    await api("/downloads/ollama", { method: "POST", json: { model: ollamaName.trim() } })
    setOllamaName("")
    refetch()
  }

  return (
    <div className="grid gap-4">
      <div className="grid gap-4 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle>Hugging Face から取得</CardTitle>
            <CardDescription>キーワード検索、または「owner/repo」を直接入力。複数接続の分割ダウンロード + 再開 + sha256 検証。</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-3">
            <form onSubmit={search} className="flex flex-wrap items-center gap-2">
              <Input className="min-w-60 flex-1" placeholder="例: Qwen3 30B GGUF / unsloth/gpt-oss-20b-GGUF" value={q} onChange={(e) => setQ(e.target.value)} />
              <div className="flex items-center gap-2">
                <Switch id="gguf" checked={ggufOnly} onCheckedChange={setGgufOnly} />
                <Label htmlFor="gguf" className="font-normal">
                  GGUF のみ
                </Label>
              </div>
              <Button type="submit" disabled={searching}>
                <SearchIcon /> 検索
              </Button>
            </form>
            {results && (
              <div className="max-h-80 overflow-auto rounded-lg border">
                <Table>
                  <TableBody>
                    {results.map((r) => (
                      <TableRow key={r.id} className="cursor-pointer" onClick={() => setRepo(r.id)}>
                        <TableCell className="font-medium">{r.id}</TableCell>
                        <TableCell className="text-right text-xs text-muted-foreground tabular-nums">
                          <DownloadIcon className="mr-1 inline size-3" />
                          {r.downloads.toLocaleString()}
                        </TableCell>
                        <TableCell className="w-16 text-right text-xs text-muted-foreground tabular-nums">
                          <HeartIcon className="mr-1 inline size-3" />
                          {r.likes}
                        </TableCell>
                      </TableRow>
                    ))}
                    {results.length === 0 && (
                      <TableRow>
                        <TableCell className="text-muted-foreground">見つかりませんでした</TableCell>
                      </TableRow>
                    )}
                  </TableBody>
                </Table>
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>Ollama pull</CardTitle>
            <CardDescription>Ollama のレジストリから取得 (Ollama 起動中のみ)</CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={pullOllama} className="flex gap-2">
              <Input placeholder="qwen3:30b-a3b" value={ollamaName} onChange={(e) => setOllamaName(e.target.value)} />
              <Button type="submit" variant="secondary">
                Pull
              </Button>
            </form>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>キュー</CardTitle>
          <CardDescription>WSL を落としても途中から再開します</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-3">
          {(queue ?? []).length === 0 && <p className="text-sm text-muted-foreground">ダウンロード履歴はありません</p>}
          {(queue ?? []).map((d) => (
            <div key={d.id} className="grid gap-2 rounded-lg border p-3">
              <div className="flex flex-wrap items-center gap-2">
                <Badge variant="outline">{d.kind === "hf" ? "HF" : "Ollama"}</Badge>
                <span className="font-medium">{d.repo}</span>
                <StatusBadge status={d.status} />
                <span className="ml-auto text-xs text-muted-foreground tabular-nums">
                  {bytes(d.downloaded)} / {bytes(d.total)}
                  {d.status === "downloading" && ` · ${bytes(d.speed)}/s · 残り ${duration(d.eta)}`}
                </span>
                <div className="flex gap-1">
                  {(d.status === "downloading" || d.status === "queued" || d.status === "verifying") && (
                    <Button size="icon-sm" variant="ghost" title="一時停止" onClick={() => act(d.id, "pause")}>
                      <PauseIcon />
                    </Button>
                  )}
                  {(d.status === "paused" || d.status === "failed" || d.status === "cancelled") && (
                    <Button size="icon-sm" variant="ghost" title="再開" onClick={() => act(d.id, "resume")}>
                      <PlayIcon />
                    </Button>
                  )}
                  {d.status !== "completed" && d.status !== "cancelled" && (
                    <Button size="icon-sm" variant="ghost" title="キャンセル (部分ファイル削除)" onClick={() => act(d.id, "cancel")}>
                      <XIcon />
                    </Button>
                  )}
                  <Button size="icon-sm" variant="ghost" title="履歴から削除" onClick={() => act(d.id, "delete")}>
                    <Trash2Icon />
                  </Button>
                </div>
              </div>
              <Progress value={pct(d.downloaded, d.total)} className="[&_[data-slot=progress-track]]:h-2" />
              {d.kind === "ollama" && d.ollama_status && <div className="text-xs text-muted-foreground">{d.ollama_status}</div>}
              {d.files.length > 1 && (
                <div className="grid gap-1 pl-2">
                  {d.files.map((f) => (
                    <div key={f.path} className="flex items-center gap-2 text-xs text-muted-foreground">
                      <span className="truncate">{f.path}</span>
                      <span className="ml-auto tabular-nums">{f.complete ? "✓" : `${pct(f.downloaded, f.size).toFixed(0)}%`}</span>
                    </div>
                  ))}
                </div>
              )}
              {d.error && <div className="text-xs text-red-500">{d.error}</div>}
            </div>
          ))}
        </CardContent>
      </Card>

      <RepoFilesDialog repo={repo} onClose={() => setRepo(null)} onQueued={refetch} />
    </div>
  )
}

function RepoFilesDialog({ repo, onClose, onQueued }: { repo: string | null; onClose: () => void; onQueued: () => void }) {
  const { data: files, error } = usePoll<HfFile[]>(repo ? `/hf/files/${repo}` : null, 0)
  const [picked, setPicked] = useState<Set<string>>(new Set())

  // Group split GGUF shards and same-quant files so one click selects all parts.
  const groups = useMemo(() => {
    const out = new Map<string, HfFile[]>()
    for (const f of files ?? []) {
      if (!f.size || f.path.startsWith(".")) continue
      const isGguf = f.path.endsWith(".gguf")
      const key = isGguf ? (f.path.includes("mmproj") ? f.path : f.split ? f.path.replace(/-\d{5}-of-\d{5}\.gguf$/, "") : f.path) : "__repo__"
      out.set(key, [...(out.get(key) ?? []), f])
    }
    return [...out.entries()].sort(([a], [b]) => (a === "__repo__" ? 1 : b === "__repo__" ? -1 : a.localeCompare(b)))
  }, [files])

  const selected = groups.filter(([k]) => picked.has(k)).flatMap(([, fs]) => fs)
  const total = selected.reduce((s, f) => s + f.size, 0)

  function toggle(k: string) {
    setPicked((p) => {
      const n = new Set(p)
      if (n.has(k)) n.delete(k)
      else n.add(k)
      return n
    })
  }

  async function queue() {
    try {
      await api("/downloads/hf", { method: "POST", json: { repo, files: selected } })
      toast.success(`${selected.length} ファイルをキューに追加`)
      setPicked(new Set())
      onQueued()
      onClose()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  return (
    <Dialog open={!!repo} onOpenChange={(o) => !o && (setPicked(new Set()), onClose())}>
      <DialogContent className="max-h-[85vh] overflow-hidden sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle className="truncate">{repo}</DialogTitle>
          <DialogDescription>量子化を選択 (分割ファイルはまとめて選択されます)。safetensors は「リポジトリ全体」で vLLM 用に取得。</DialogDescription>
        </DialogHeader>
        {error && <p className="text-sm text-red-500">{error}</p>}
        <div className="max-h-[55vh] overflow-auto rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-8" />
                <TableHead>ファイル</TableHead>
                <TableHead>量子化</TableHead>
                <TableHead className="text-right">サイズ</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {groups.map(([k, fs]) => (
                <TableRow key={k} className="cursor-pointer" onClick={() => toggle(k)}>
                  <TableCell>
                    <Checkbox checked={picked.has(k)} onCheckedChange={() => toggle(k)} onClick={(e) => e.stopPropagation()} />
                  </TableCell>
                  <TableCell className="max-w-md truncate font-mono text-xs">
                    {k === "__repo__" ? `リポジトリ全体 (${fs.length} files)` : fs.length > 1 ? `${k}  [${fs.length} parts]` : k}
                  </TableCell>
                  <TableCell>{fs[0].quant && <Badge variant="secondary">{fs[0].quant}</Badge>}</TableCell>
                  <TableCell className="text-right tabular-nums">{bytes(fs.reduce((s, f) => s + f.size, 0))}</TableCell>
                </TableRow>
              ))}
              {!files && !error && (
                <TableRow>
                  <TableCell colSpan={4} className="text-muted-foreground">
                    読み込み中…
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
        <DialogFooter className="items-center">
          <span className="mr-auto text-sm text-muted-foreground">
            {selected.length} ファイル · {bytes(total)}
          </span>
          <Button onClick={queue} disabled={!selected.length}>
            <DownloadIcon /> ダウンロード
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
