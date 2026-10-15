import { RefreshCwIcon } from "lucide-react"

import { ErrorAlert, LoadingLines } from "@/components/admin/shared"
import type { AuditEntry } from "@/components/admin/types"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/lib/api"
import { dateTime } from "@/lib/format"

export function AuditTab() {
  const { data, error, refetch } = usePoll<{ entries: AuditEntry[] }>("/admin/audit?limit=200", 0)

  return (
    <Card>
      <CardHeader>
        <CardTitle>監査ログ</CardTitle>
        <CardDescription>管理操作の履歴 (新しい順・最大 200 件)。値は記録されず、変更されたキー名などだけが残ります。</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        <div>
          <Button variant="outline" size="sm" onClick={() => refetch()}>
            <RefreshCwIcon /> 更新
          </Button>
        </div>
        {error && <ErrorAlert error={error} />}
        {!data && !error && <LoadingLines rows={6} />}
        {data &&
          (data.entries.length === 0 ? (
            <p className="text-sm text-muted-foreground">監査ログはまだありません。</p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="whitespace-nowrap">日時</TableHead>
                  <TableHead>ユーザー</TableHead>
                  <TableHead>IP</TableHead>
                  <TableHead className="min-w-48">操作</TableHead>
                  <TableHead className="min-w-56">概要</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.entries.map((e, i) => (
                  <TableRow key={`${e.ts}-${i}`}>
                    <TableCell className="whitespace-nowrap tabular-nums">{dateTime(e.ts)}</TableCell>
                    <TableCell>{e.user}</TableCell>
                    <TableCell className="font-mono text-xs">{e.ip}</TableCell>
                    <TableCell className="font-mono text-xs whitespace-nowrap">{e.action}</TableCell>
                    <TableCell className="max-w-md break-words whitespace-normal">{e.summary}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          ))}
      </CardContent>
    </Card>
  )
}
