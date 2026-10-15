import { useState } from "react"
import { ClipboardListIcon, KeyRoundIcon, MonitorIcon, PlayIcon, ScrollTextIcon, SettingsIcon } from "lucide-react"

import { ActionsTab } from "@/components/admin/actions-tab"
import { AuditTab } from "@/components/admin/audit-tab"
import { ConfigTab } from "@/components/admin/config-tab"
import { EnvTab } from "@/components/admin/env-tab"
import { LogsTab } from "@/components/admin/logs-tab"
import { SystemTab } from "@/components/admin/system-tab"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

const TABS = [
  { id: "config", label: "設定", icon: SettingsIcon },
  { id: "env", label: "環境変数", icon: KeyRoundIcon },
  { id: "system", label: "システム", icon: MonitorIcon },
  { id: "actions", label: "アクション", icon: PlayIcon },
  { id: "logs", label: "ログ", icon: ScrollTextIcon },
  { id: "audit", label: "監査ログ", icon: ClipboardListIcon },
] as const

type TabId = (typeof TABS)[number]["id"]

/** Remote administration: settings, env, system status, maintenance actions, logs and audit. */
export function AdminPage() {
  const [tab, setTab] = useState<TabId>("config")
  // Kept here so the job log survives switching to another admin tab and back.
  const [jobId, setJobId] = useState<string | null>(null)

  return (
    <Tabs value={tab} onValueChange={(v) => setTab(String(v) as TabId)} className="min-w-0 gap-4">
      <div className="-mx-3 overflow-x-auto px-3 sm:mx-0 sm:px-0">
        <TabsList className="h-auto w-max max-w-none justify-start">
          {TABS.map((t) => (
            <TabsTrigger key={t.id} value={t.id} className="h-8 px-2.5">
              <t.icon />
              {t.label}
            </TabsTrigger>
          ))}
        </TabsList>
      </div>

      <TabsContent value="config" className="min-w-0">
        <ConfigTab />
      </TabsContent>
      <TabsContent value="env" className="min-w-0">
        <EnvTab />
      </TabsContent>
      <TabsContent value="system" className="min-w-0">
        <SystemTab />
      </TabsContent>
      <TabsContent value="actions" className="min-w-0">
        <ActionsTab jobId={jobId} onJob={setJobId} />
      </TabsContent>
      <TabsContent value="logs" className="min-w-0">
        <LogsTab />
      </TabsContent>
      <TabsContent value="audit" className="min-w-0">
        <AuditTab />
      </TabsContent>
    </Tabs>
  )
}
