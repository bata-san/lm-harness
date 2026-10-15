import { useEffect, useRef, useState } from "react"
import { RefreshCwIcon } from "lucide-react"

import { ErrorAlert, LoadingLines } from "@/components/admin/shared"
import type { LogSource, LogsResponse } from "@/components/admin/types"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { usePoll } from "@/lib/api"

const FALLBACK: LogSource[] = [
  { id: "harness", title: "ハーネス本体" },
  { id: "tunnel", title: "トンネル" },
]

export function LogsTab() {
  const sources = usePoll<{ sources: LogSource[] }>("/admin/logs/sources", 10000)
  const [source, setSource] = useState("harness")
  const [auto, setAuto] = useState(false)
  const list = sources.data?.sources ?? FALLBACK
  const title = (id: string) => list.find((s) => s.id === id)?.title ?? id
  const logs = usePoll<LogsResponse>(`/admin/logs?source=${encodeURIComponent(source)}&lines=200`, auto ? 3000 : 0)
  const box = useRef<HTMLPreElement>(null)

  // Keep the newest line in view whenever the content changes.
  useEffect(() => {
    if (box.current) box.current.scrollTop = box.current.scrollHeight
  }, [logs.data])

  return (
    <Card>
      <CardHeader>
        <CardTitle>ログ</CardTitle>
        <CardDescription>各ソースの末尾 200 行を表示します。</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        <div className="flex flex-col gap-3 sm:flex-row sm:flex-wrap sm:items-end">
          <div className="grid min-w-0 gap-1.5 sm:w-72">
            <Label>ソース</Label>
            <Select value={source} onValueChange={(v) => v && setSource(String(v))}>
              <SelectTrigger className="w-full">
                <SelectValue>{(v: string) => title(v)}</SelectValue>
              </SelectTrigger>
              <SelectContent>
                {list.map((s) => (
                  <SelectItem key={s.id} value={s.id}>
                    {s.title}
                    {s.status ? ` (${s.status})` : ""}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="flex items-center gap-3">
            <div className="flex items-center gap-2">
              <Switch id="log-auto" checked={auto} onCheckedChange={setAuto} />
              <Label htmlFor="log-auto">自動更新 (3 秒)</Label>
            </div>
            <Button variant="outline" size="sm" onClick={() => logs.refetch()}>
              <RefreshCwIcon /> 更新
            </Button>
          </div>
        </div>

        {sources.error && <ErrorAlert error={sources.error} title="ソース一覧を取得できません" />}
        {logs.error && <ErrorAlert error={logs.error} title="ログを取得できません" />}
        {!logs.data && !logs.error && <LoadingLines rows={5} />}
        {logs.data && (
          <pre
            ref={box}
            className="h-[60vh] min-h-64 overflow-auto rounded-lg border bg-zinc-950 p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap break-all text-zinc-200"
          >
            {logs.data.lines.length ? logs.data.lines.join("\n") : "ログはありません。"}
          </pre>
        )}
      </CardContent>
    </Card>
  )
}
