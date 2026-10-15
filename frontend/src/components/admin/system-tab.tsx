import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { ErrorAlert, LoadingLines } from "@/components/admin/shared"
import type { DirInfo, SystemInfo } from "@/components/admin/types"
import { Meter, KV } from "@/components/stat"
import { usePoll, type ModeName } from "@/lib/api"
import { bytes, dateTime, duration, pct } from "@/lib/format"
import { MODE_LABEL, gb } from "@/lib/mode"

const DIR_LABEL: Record<string, string> = {
  harness_home: "ハーネス home",
  models_dir: "モデル",
  llama_cpp_dir: "llama.cpp",
  vllm_venv: "vLLM venv",
  strata_dir: "Strata",
}

export function SystemTab() {
  const { data, error } = usePoll<SystemInfo>("/admin/system", 5000)

  if (!data) {
    return error ? <ErrorAlert error={error} /> : <LoadingLines rows={6} />
  }

  return (
    <div className="grid gap-4">
      {error && <ErrorAlert error={error} title="最新の状態を取得できません (表示は直前の値)" />}

      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>ホスト</CardTitle>
          </CardHeader>
          <CardContent className="grid grid-cols-2 gap-3">
            <KV k="ホスト名" v={data.hostname ?? "—"} className="col-span-2 min-w-0" />
            <KV k="OS" v={data.os ?? "—"} className="col-span-2 min-w-0" />
            <KV k="カーネル" v={data.kernel ?? "—"} className="min-w-0" />
            <KV k="稼働時間" v={duration(data.uptime_secs)} />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>ハーネス</CardTitle>
            <CardDescription>
              v{data.harness.version} · PID {data.harness.pid}
            </CardDescription>
          </CardHeader>
          <CardContent className="grid grid-cols-2 gap-3">
            <KV k="起動時刻" v={dateTime(data.harness.started_at)} />
            <KV k="稼働時間" v={duration(data.harness.uptime_secs)} />
            <KV
              k="実行方式"
              v={
                <Badge variant={data.harness.under_systemd ? "secondary" : "outline"}>
                  {data.harness.under_systemd ? "systemd 配下" : "手動起動"}
                </Badge>
              }
            />
            <KV k="ホーム" v={<span className="font-mono text-xs break-all">{data.harness.home}</span>} className="col-span-2 min-w-0" />
            <KV k="リポジトリ" v={<span className="font-mono text-xs break-all">{data.harness.repo_root}</span>} className="col-span-2 min-w-0" />
            <KV k="バイナリ" v={<span className="font-mono text-xs break-all">{data.harness.binary ?? "—"}</span>} className="col-span-2 min-w-0" />
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>systemd</CardTitle>
          </CardHeader>
          <CardContent>
            {data.systemd.available ? (
              <div className="grid grid-cols-2 gap-3">
                <KV
                  k="状態"
                  v={
                    <Badge variant={data.systemd.active_state === "active" ? "secondary" : "destructive"}>
                      {data.systemd.active_state ?? "—"} {data.systemd.sub_state ? `(${data.systemd.sub_state})` : ""}
                    </Badge>
                  }
                />
                <KV k="スコープ" v={data.systemd.scope === "user" ? "ユーザー" : "システム"} />
                <KV k="MainPID" v={data.systemd.main_pid ?? "—"} />
                <KV k="再起動回数" v={data.systemd.restarts ?? "—"} />
                <KV k="ユニット" v={<span className="font-mono text-xs">{data.systemd.unit}</span>} />
                <KV k="自動起動" v={data.systemd.unit_file_state ?? "—"} />
                <KV k="Restart ポリシー" v={data.systemd.restart_policy ?? "—"} className="col-span-2" />
              </div>
            ) : (
              <p className="text-sm text-muted-foreground">llm-harness ユニットは見つかりません (./harness.sh service で登録できます)。</p>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>トンネルとモード</CardTitle>
          </CardHeader>
          <CardContent className="grid grid-cols-2 gap-3">
            <KV
              k="トンネル"
              v={
                <Badge variant={data.tunnel.state === "connected" ? "secondary" : data.tunnel.state === "error" ? "destructive" : "outline"}>
                  {data.tunnel.state ?? "—"}
                </Badge>
              }
            />
            <KV k="プロバイダ" v={data.tunnel.provider ?? "—"} />
            <KV k="公開 URL" v={<span className="font-mono text-xs break-all">{data.tunnel.public_url ?? "—"}</span>} className="col-span-2 min-w-0" />
            {data.tunnel.last_error && (
              <p className="col-span-2 text-xs break-all text-destructive">直近のエラー: {data.tunnel.last_error}</p>
            )}
            <KV k="現在のモード" v={MODE_LABEL[data.mode.current as ModeName] ?? data.mode.current} />
            <KV k="VRAM 予約" v={gb(data.mode.profile.vram_reserve_mb)} />
            <KV
              k="CPU"
              v={`${data.mode.profile.cpu_set.length} / ${data.mode.profile.logical_cpus} 論理コア`}
            />
            <KV k="GPU 電力上限" v={`${data.mode.profile.gpu_power_percent}%`} />
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle>ディレクトリ</CardTitle>
          <CardDescription>使用量は上限件数・時間内の集計です。ディスクの使用率はファイルシステム全体の値です。</CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>用途</TableHead>
                <TableHead className="min-w-56">パス</TableHead>
                <TableHead>使用量</TableHead>
                <TableHead className="min-w-44">ディスク</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {data.dirs.map((d) => (
                <DirRow key={d.key} dir={d} />
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>子プロセス</CardTitle>
          <CardDescription>ハーネスが起動したプロセス ({data.processes.length} 件)</CardDescription>
        </CardHeader>
        <CardContent>
          {data.processes.length === 0 ? (
            <p className="text-sm text-muted-foreground">起動中のプロセスはありません。</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>PID</TableHead>
                  <TableHead>親 PID</TableHead>
                  <TableHead>メモリ</TableHead>
                  <TableHead className="min-w-64">コマンド</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.processes.map((p) => (
                  <TableRow key={p.pid}>
                    <TableCell className="tabular-nums">{p.pid}</TableCell>
                    <TableCell className="tabular-nums">{p.ppid ?? "—"}</TableCell>
                    <TableCell className="tabular-nums">{bytes(p.rss_bytes)}</TableCell>
                    <TableCell className="max-w-md font-mono text-xs break-all whitespace-normal">{p.cmd}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  )
}

function DirRow({ dir }: { dir: DirInfo }) {
  const used = dir.disk_total_bytes != null && dir.disk_free_bytes != null ? dir.disk_total_bytes - dir.disk_free_bytes : null
  const diskPct = used != null && dir.disk_total_bytes ? pct(used, dir.disk_total_bytes) : null
  return (
    <TableRow>
      <TableCell className="align-top">
        <div className="font-medium">{DIR_LABEL[dir.key] ?? dir.key}</div>
        {!dir.exists && <Badge variant="outline" className="mt-1">未作成</Badge>}
      </TableCell>
      <TableCell className="min-w-56 max-w-80 font-mono text-xs break-all whitespace-normal align-top">{dir.path}</TableCell>
      <TableCell className="align-top tabular-nums whitespace-nowrap">
        {dir.exists ? bytes(dir.used_bytes) : "—"}
        {dir.truncated && <span className="ml-1 text-[10px] text-muted-foreground">(一部)</span>}
      </TableCell>
      <TableCell className="min-w-64 align-top">
        {diskPct != null ? (
          <Meter label="使用率" value={diskPct} detail={`空き ${bytes(dir.disk_free_bytes)} / ${bytes(dir.disk_total_bytes)}`} />
        ) : (
          <span className="text-xs text-muted-foreground">—</span>
        )}
      </TableCell>
    </TableRow>
  )
}
