import { useState } from "react"
import { CopyIcon, PlayIcon, PlusIcon, ScrollTextIcon, SquareIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { LaunchDialog } from "@/components/launch-dialog"
import { LogViewer } from "@/components/log-viewer"
import { KV } from "@/components/stat"
import { StatusBadge } from "@/components/status-badge"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { api, usePoll, type Instance, type Profile } from "@/lib/api"
import { since } from "@/lib/format"
import { MODE_LABEL, MODE_STYLE } from "@/lib/mode"
import { cn } from "@/lib/utils"

export function ServersPage() {
  const { data: instances, refetch } = usePoll<Instance[]>("/instances", 1500)
  const { data: profiles, refetch: refetchProfiles } = usePoll<Profile[]>("/profiles", 0)
  const [launch, setLaunch] = useState<Partial<Profile> | null>(null)
  const [logs, setLogs] = useState<Instance | null>(null)

  async function stop(i: Instance) {
    await api(`/instances/${i.id}/stop`, { method: "POST" })
    refetch()
  }
  async function remove(i: Instance) {
    await api(`/instances/${i.id}`, { method: "DELETE" })
    refetch()
  }
  async function runProfile(p: Profile) {
    try {
      await api("/instances", { method: "POST", json: p })
      toast.success(`${p.name} を起動`)
      refetch()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }
  async function deleteProfile(p: Profile) {
    await api(`/profiles/${p.id}`, { method: "DELETE" })
    refetchProfiles()
  }
  function copy(text: string) {
    navigator.clipboard.writeText(text)
    toast.success("コピーしました")
  }

  return (
    <div className="grid gap-4">
      <div className="flex items-center justify-between">
        <p className="text-sm text-muted-foreground">
          WSL の mirrored ネットワークなら Windows 側のアプリからも <code className="font-mono">http://localhost:PORT/v1</code> で接続できます。
        </p>
        <Button onClick={() => setLaunch({})}>
          <PlusIcon /> 新規起動
        </Button>
      </div>

      <div className="grid gap-3">
        {(instances ?? []).length === 0 && (
          <Card>
            <CardContent className="py-8 text-center text-sm text-muted-foreground">サーバは起動していません</CardContent>
          </Card>
        )}
        {(instances ?? []).map((i) => {
          const url = `http://localhost:${i.port}/v1`
          const alive = i.status === "running" || i.status === "starting"
          return (
            <Card key={i.id} size="sm">
              <CardContent className="grid gap-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{i.name}</span>
                  <Badge variant="outline">{i.runtime}</Badge>
                  <StatusBadge status={i.status} />
                  {i.mode && (
                    <Badge variant="outline" className={cn(MODE_STYLE[i.mode].text, MODE_STYLE[i.mode].border)} title="起動時のモード">
                      {MODE_LABEL[i.mode]}で起動
                    </Badge>
                  )}
                  {i.auto_tune && <Badge variant="secondary">自動チューニング</Badge>}
                  {alive && i.stale && (
                    <Badge className="bg-amber-500/15 text-amber-700 dark:text-amber-400" title="現在のモードと異なる設定で稼働中です">
                      モード不一致 — 再起動で反映
                    </Badge>
                  )}
                  <span className="text-xs text-muted-foreground">
                    pid {i.pid ?? "—"} · {since(i.started_at)}
                    {i.returncode != null && ` · exit ${i.returncode}`}
                  </span>
                  <div className="ml-auto flex gap-1">
                    <Button size="sm" variant="ghost" onClick={() => copy(url)}>
                      <CopyIcon /> <span className="font-mono text-xs">{url}</span>
                    </Button>
                    <Button size="icon-sm" variant="ghost" title="ログ" onClick={() => setLogs(i)}>
                      <ScrollTextIcon />
                    </Button>
                    {alive ? (
                      <Button size="icon-sm" variant="ghost" title="停止" onClick={() => stop(i)}>
                        <SquareIcon />
                      </Button>
                    ) : (
                      <>
                        <Button
                          size="icon-sm"
                          variant="ghost"
                          title="同じ設定で再起動"
                          onClick={() =>
                            setLaunch({ runtime: i.runtime, model: i.model, name: i.name, port: i.port, args: i.args, auto_tune: i.auto_tune })
                          }
                        >
                          <PlayIcon />
                        </Button>
                        <Button size="icon-sm" variant="ghost" title="一覧から削除" onClick={() => remove(i)}>
                          <Trash2Icon />
                        </Button>
                      </>
                    )}
                  </div>
                </div>
                {i.runtime !== "ollama" && (
                  <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
                    <KV k="生成 tok/s" v={i.metrics.gen_tps?.toFixed(1) ?? "—"} />
                    <KV k="プロンプト tok/s" v={i.metrics.prompt_tps?.toFixed(0) ?? "—"} />
                    <KV k="処理中 / 待ち" v={`${i.metrics.processing ?? "—"} / ${i.metrics.deferred ?? "—"}`} />
                    <KV k="累計生成トークン" v={i.metrics.tokens_total?.toLocaleString() ?? "—"} />
                    <KV k="KV キャッシュ" v={i.metrics.kv_usage != null ? `${(i.metrics.kv_usage * 100).toFixed(0)}%` : "—"} />
                  </div>
                )}
                <code className="block truncate rounded bg-muted px-2 py-1 font-mono text-[11px] text-muted-foreground" title={i.cmd.join(" ")}>
                  {i.cmd.join(" ")}
                </code>
              </CardContent>
            </Card>
          )
        })}
      </div>

      <Card>
        <CardHeader>
          <CardTitle>プロファイル</CardTitle>
          <CardDescription>よく使う起動設定。自動起動を ON にするとハーネス起動時に立ち上がります。</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-2">
          {(profiles ?? []).length === 0 && <p className="text-sm text-muted-foreground">保存済みプロファイルはありません</p>}
          {(profiles ?? []).map((p) => (
            <div key={p.id} className="flex flex-wrap items-center gap-2 rounded-lg border p-2 pl-3">
              <span className="font-medium">{p.name}</span>
              <Badge variant="outline">{p.runtime}</Badge>
              {p.autostart && <Badge variant="secondary">自動起動</Badge>}
              {p.auto_tune !== false && p.runtime !== "ollama" && <Badge variant="outline">自動チューニング</Badge>}
              <span className="truncate font-mono text-xs text-muted-foreground">{p.model}</span>
              <div className="ml-auto flex gap-1">
                <Button size="sm" variant="ghost" onClick={() => setLaunch(p)}>
                  編集
                </Button>
                <Button size="icon-sm" variant="ghost" title="起動" onClick={() => runProfile(p)}>
                  <PlayIcon />
                </Button>
                <Button size="icon-sm" variant="ghost" title="削除" onClick={() => deleteProfile(p)}>
                  <Trash2Icon />
                </Button>
              </div>
            </div>
          ))}
        </CardContent>
      </Card>

      <LaunchDialog
        open={!!launch}
        onOpenChange={(o) => !o && setLaunch(null)}
        initial={launch ?? undefined}
        onLaunched={() => {
          refetch()
          refetchProfiles()
        }}
      />

      <Dialog open={!!logs} onOpenChange={(o) => !o && setLogs(null)}>
        <DialogContent className="sm:max-w-5xl">
          <DialogHeader>
            <DialogTitle>{logs?.name} のログ</DialogTitle>
          </DialogHeader>
          {logs && <LogViewer path={`/instances/${logs.id}/logs`} className="h-[65vh]" />}
        </DialogContent>
      </Dialog>
    </div>
  )
}
