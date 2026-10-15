import { lazy, Suspense, useEffect, useState } from "react"
import { BoxesIcon, DownloadIcon, GaugeIcon, GlobeIcon, KeyRoundIcon, Loader2Icon, LayersIcon, LockKeyholeIcon, LogOutIcon, MessageSquareIcon, MoonIcon, ServerIcon, SettingsIcon, ShieldCheckIcon, ShieldIcon, SlidersHorizontalIcon, SunIcon, UserIcon, WrenchIcon } from "lucide-react"

import { MODE_ICON, ModeSwitch } from "@/components/mode-switch"
import { Button } from "@/components/ui/button"
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
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
  useSidebar,
} from "@/components/ui/sidebar"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { usePoll, type Download, type Instance, type SystemSnapshot } from "@/lib/api"
import { AuthGate, useAuth } from "@/lib/auth"
import { cn } from "@/lib/utils"
import { MODE_LABEL, MODE_STYLE, ModeProvider, profileSummary, useMode } from "@/lib/mode"
import { DashboardPage } from "@/pages/dashboard"
import { DownloadsPage } from "@/pages/downloads"
import { AdminPage } from "@/pages/admin"
import { ModePage } from "@/pages/mode"
import { ModelsPage } from "@/pages/models"
import { RemotePage } from "@/pages/remote"
import { RuntimesPage } from "@/pages/runtimes"
import { ServersPage } from "@/pages/servers"
import { SecurityPage } from "@/pages/security"
import { SettingsPage } from "@/pages/settings"
import { StrataPage } from "@/pages/strata"

// The chat page pulls in markdown rendering and the vault code; load it on demand.
const ChatPage = lazy(() => import("@/pages/chat").then((m) => ({ default: m.ChatPage })))

const PAGES = {
  strata: { title: "Strata", icon: LayersIcon, el: StrataPage },
  chat: { title: "チャット", icon: MessageSquareIcon, el: ChatPage },
  dashboard: { title: "ダッシュボード", icon: GaugeIcon, el: DashboardPage },
  downloads: { title: "ダウンロード", icon: DownloadIcon, el: DownloadsPage },
  models: { title: "モデル", icon: BoxesIcon, el: ModelsPage },
  servers: { title: "サーバ", icon: ServerIcon, el: ServersPage },
  mode: { title: "モード", icon: SlidersHorizontalIcon, el: ModePage },
  remote: { title: "リモートアクセス", icon: GlobeIcon, el: RemotePage },
  security: { title: "セキュリティ", icon: ShieldIcon, el: SecurityPage },
  runtimes: { title: "ランタイム", icon: WrenchIcon, el: RuntimesPage },
  settings: { title: "設定", icon: SettingsIcon, el: SettingsPage },
  admin: { title: "管理", icon: ShieldCheckIcon, el: AdminPage },
} as const
type PageId = keyof typeof PAGES

function useHashRoute(): [PageId, (p: PageId, anchor?: string) => void] {
  const read = () => {
    const h = location.hash.slice(1)
    return (h in PAGES ? h : "strata") as PageId
  }
  const [page, setPage] = useState<PageId>(read)
  useEffect(() => {
    const on = () => setPage(read())
    addEventListener("hashchange", on)
    return () => removeEventListener("hashchange", on)
  }, [])
  return [
    page,
    (p, anchor) => {
      location.hash = p
      // Scroll to a section of the target page once it has rendered.
      if (anchor) setTimeout(() => document.getElementById(anchor)?.scrollIntoView({ behavior: "smooth", block: "start" }), 150)
    },
  ]
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

/** Sidebar entries; closes the mobile sheet after navigating. */
function NavItems({ page, go, badges }: { page: PageId; go: (p: PageId) => void; badges: Partial<Record<PageId, number>> }) {
  const { isMobile, setOpenMobile } = useSidebar()
  return (
    <SidebarMenu>
      {(Object.keys(PAGES) as PageId[]).map((id) => {
        const P = PAGES[id]
        return (
          <SidebarMenuItem key={id}>
            <SidebarMenuButton
              isActive={page === id}
              tooltip={P.title}
              onClick={() => {
                go(id)
                if (isMobile) setOpenMobile(false)
              }}
            >
              <P.icon />
              <span>{P.title}</span>
            </SidebarMenuButton>
            {badges[id] ? <SidebarMenuBadge>{badges[id]}</SidebarMenuBadge> : null}
          </SidebarMenuItem>
        )
      })}
    </SidebarMenu>
  )
}

function UserMenu({ go }: { go: (p: PageId, anchor?: string) => void }) {
  const { status, logout } = useAuth()
  const [pending, setPending] = useState(false)
  const isKey = status.principal_kind === "api_key" || status.principal_kind === "local"
  return (
    <DropdownMenu>
      <DropdownMenuTrigger render={<Button variant="ghost" size="sm" className="max-w-40 gap-1.5 px-2" aria-label="ユーザーメニュー" />}>
        <UserIcon />
        <span className="hidden truncate sm:inline">{status.user ?? "ユーザー"}</span>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuGroup>
          <DropdownMenuLabel className="truncate">
            {status.user ?? "ユーザー"}
            {status.local && <span className="ml-1 font-normal text-muted-foreground">(ローカル)</span>}
          </DropdownMenuLabel>
        </DropdownMenuGroup>
        <DropdownMenuSeparator />
        <DropdownMenuItem onClick={() => go("security", "password")}>
          <LockKeyholeIcon /> パスワード変更
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => go("security", "totp")}>
          <ShieldCheckIcon /> 二段階認証
          {!status.totp_enabled && <span className="ml-auto text-[10px] text-amber-600 dark:text-amber-400">未設定</span>}
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => go("security", "keys")}>
          <KeyRoundIcon /> API キー
        </DropdownMenuItem>
        {!isKey && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              disabled={pending}
              onClick={async () => {
                setPending(true)
                try {
                  await logout()
                } finally {
                  setPending(false)
                }
              }}
            >
              <LogOutIcon /> ログアウト
            </DropdownMenuItem>
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function Shell() {
  const { state: mode } = useMode()
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
                <NavItems page={page} go={setPage} badges={badges} />
              </SidebarGroupContent>
            </SidebarGroup>
          </SidebarContent>
          <SidebarFooter className="group-data-[collapsible=icon]:hidden">
            {mode && (
              <a href="#mode" className={cn("rounded-lg border p-2 text-xs", MODE_STYLE[mode.mode].border, MODE_STYLE[mode.mode].bg)}>
                <div className={cn("flex items-center gap-1.5 font-medium", MODE_STYLE[mode.mode].text)}>
                  {mode.mode === "full" ? <MODE_ICON.full className="size-3.5" /> : <MODE_ICON.shared className="size-3.5" />}
                  {MODE_LABEL[mode.mode]}
                </div>
                <div className="text-muted-foreground tabular-nums">{profileSummary(mode)}</div>
              </a>
            )}
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
          <header className="sticky top-0 z-10 flex h-14 items-center gap-2 border-b bg-background/80 px-3 backdrop-blur sm:px-4">
            <SidebarTrigger className="-ml-1" />
            <Separator orientation="vertical" className="mr-1 data-[orientation=vertical]:h-4 sm:mr-2" />
            <h1 className="min-w-0 truncate font-medium">{PAGES[page].title}</h1>
            <div className="ml-auto flex shrink-0 items-center gap-1.5 sm:gap-3">
              <ModeSwitch />
              <ThemeToggle />
              <UserMenu go={setPage} />
            </div>
          </header>
          <main className="min-w-0 p-3 sm:p-4 md:p-6">
            <Suspense
              fallback={
                <div className="flex justify-center py-16 text-muted-foreground">
                  <Loader2Icon className="animate-spin" />
                </div>
              }
            >
              <Page />
            </Suspense>
          </main>
        </SidebarInset>
      </SidebarProvider>
    </TooltipProvider>
  )
}

export default function App() {
  return (
    <>
      <AuthGate>
        <ModeProvider>
          <Shell />
        </ModeProvider>
      </AuthGate>
      <Toaster richColors position="bottom-right" />
    </>
  )
}
