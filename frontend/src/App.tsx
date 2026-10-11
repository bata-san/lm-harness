import { useEffect, useState } from "react"
import { BoxesIcon, DownloadIcon, GaugeIcon, MoonIcon, ServerIcon, SettingsIcon, SunIcon, WrenchIcon } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Separator } from "@/components/ui/separator"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
} from "@/components/ui/sidebar"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { usePoll, type Download, type Instance, type SystemSnapshot } from "@/lib/api"
import { DashboardPage } from "@/pages/dashboard"
import { DownloadsPage } from "@/pages/downloads"
import { ModelsPage } from "@/pages/models"
import { RuntimesPage } from "@/pages/runtimes"
import { ServersPage } from "@/pages/servers"
import { SettingsPage } from "@/pages/settings"

const PAGES = {
  dashboard: { title: "ダッシュボード", icon: GaugeIcon, el: DashboardPage },
  downloads: { title: "ダウンロード", icon: DownloadIcon, el: DownloadsPage },
  models: { title: "モデル", icon: BoxesIcon, el: ModelsPage },
  servers: { title: "サーバ", icon: ServerIcon, el: ServersPage },
  runtimes: { title: "ランタイム", icon: WrenchIcon, el: RuntimesPage },
  settings: { title: "設定", icon: SettingsIcon, el: SettingsPage },
} as const
type PageId = keyof typeof PAGES

function useHashRoute(): [PageId, (p: PageId) => void] {
  const read = () => {
    const h = location.hash.slice(1)
    return (h in PAGES ? h : "dashboard") as PageId
  }
  const [page, setPage] = useState<PageId>(read)
  useEffect(() => {
    const on = () => setPage(read())
    addEventListener("hashchange", on)
    return () => removeEventListener("hashchange", on)
  }, [])
  return [page, (p) => (location.hash = p)]
}

function ThemeToggle() {
  const [dark, setDark] = useState(() => document.documentElement.classList.contains("dark"))
  return (
    <Button
      variant="ghost"
      size="icon-sm"
      title="テーマ切替"
      onClick={() => {
        const next = !dark
        document.documentElement.classList.toggle("dark", next)
        try {
          localStorage.setItem("theme", next ? "dark" : "light")
        } catch {
          /* storage unavailable */
        }
        setDark(next)
      }}
    >
      {dark ? <SunIcon /> : <MoonIcon />}
    </Button>
  )
}

export default function App() {
  const [page, setPage] = useHashRoute()
  const { data: sys } = usePoll<SystemSnapshot>("/system", 3000)
  const { data: dls } = usePoll<Download[]>("/downloads", 3000)
  const { data: inst } = usePoll<Instance[]>("/instances", 3000)
  const badges: Partial<Record<PageId, number>> = {
    downloads: (dls ?? []).filter((d) => ["downloading", "queued", "verifying"].includes(d.status)).length,
    servers: (inst ?? []).filter((i) => i.status === "running" || i.status === "starting").length,
  }
  const gpu = sys?.gpus[0]
  const Page = PAGES[page].el

  return (
    <TooltipProvider>
      <SidebarProvider>
        <Sidebar collapsible="icon">
          <SidebarHeader>
            <div className="flex items-center gap-2 px-1 py-1.5">
              <img src="/favicon.svg" className="size-7 shrink-0" alt="" />
              <div className="grid leading-tight group-data-[collapsible=icon]:hidden">
                <span className="font-semibold">LLM Harness</span>
                <span className="text-xs text-muted-foreground">local inference control</span>
              </div>
            </div>
          </SidebarHeader>
          <SidebarContent>
            <SidebarGroup>
              <SidebarGroupContent>
                <SidebarMenu>
                  {(Object.keys(PAGES) as PageId[]).map((id) => {
                    const P = PAGES[id]
                    return (
                      <SidebarMenuItem key={id}>
                        <SidebarMenuButton isActive={page === id} tooltip={P.title} onClick={() => setPage(id)}>
                          <P.icon />
                          <span>{P.title}</span>
                        </SidebarMenuButton>
                        {badges[id] ? <SidebarMenuBadge>{badges[id]}</SidebarMenuBadge> : null}
                      </SidebarMenuItem>
                    )
                  })}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>
          </SidebarContent>
          <SidebarFooter className="group-data-[collapsible=icon]:hidden">
            {gpu && (
              <div className="rounded-lg border p-2 text-xs">
                <div className="truncate font-medium">{gpu.name}</div>
                <div className="text-muted-foreground tabular-nums">
                  VRAM {(gpu.memory_used_mb / 1024).toFixed(1)}/{(gpu.memory_total_mb / 1024).toFixed(0)} GB · {gpu.util_gpu}% · {gpu.temperature}°C
                </div>
              </div>
            )}
          </SidebarFooter>
        </Sidebar>
        <SidebarInset>
          <header className="sticky top-0 z-10 flex h-14 items-center gap-2 border-b bg-background/80 px-4 backdrop-blur">
            <SidebarTrigger className="-ml-1" />
            <Separator orientation="vertical" className="mr-2 data-[orientation=vertical]:h-4" />
            <h1 className="font-medium">{PAGES[page].title}</h1>
            <div className="ml-auto">
              <ThemeToggle />
            </div>
          </header>
          <main className="p-4 md:p-6">
            <Page />
          </main>
        </SidebarInset>
      </SidebarProvider>
      <Toaster richColors position="bottom-right" />
    </TooltipProvider>
  )
}
