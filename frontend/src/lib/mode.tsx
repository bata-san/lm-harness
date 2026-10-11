import { createContext, useCallback, useContext, useState, type ReactNode } from "react"
import { toast } from "sonner"

import { api, usePoll, type ModeName, type ModeSettings, type ModeSource, type ModeState } from "@/lib/api"

type Ctx = {
  state: ModeState | undefined
  /** Mode shown in the UI (optimistic while a switch is in flight). */
  mode: ModeName | undefined
  pending: boolean
  setMode: (m: ModeName) => Promise<void>
  saveSettings: (patch: Partial<ModeSettings>) => Promise<boolean>
  refetch: () => void
}

const ModeContext = createContext<Ctx>({
  state: undefined,
  mode: undefined,
  pending: false,
  setMode: async () => {},
  saveSettings: async () => false,
  refetch: () => {},
})

export const MODE_LABEL: Record<ModeName, string> = { full: "フル性能", shared: "共有モード" }
export const SOURCE_LABEL: Record<ModeSource, string> = { manual: "手動", schedule: "スケジュール", startup: "起動時", settings: "設定" }

export function ModeProvider({ children }: { children: ReactNode }) {
  const { data, refetch } = usePoll<ModeState>("/mode", 3000)
  const [optimistic, setOptimistic] = useState<ModeName | null>(null)
  const [override, setOverride] = useState<ModeState | undefined>()

  // Prefer the response of our last PUT until the poll catches up with it.
  const state = override && (!data || (override.switched_at ?? 0) > (data.switched_at ?? 0)) ? override : data

  const setMode = useCallback(
    async (m: ModeName) => {
      if (optimistic !== null || m === state?.mode) return
      setOptimistic(m)
      try {
        const r = await api<ModeState>("/mode", { method: "PUT", json: { mode: m } })
        setOverride(r)
        toast.success(`${MODE_LABEL[m]}に切り替えました`, {
          description: r.restarted?.length ? `${r.restarted.length} 台のサーバを新しい設定で再起動` : undefined,
        })
        refetch()
      } catch (e) {
        // optimistic value is dropped in finally => UI rolls back to the real mode
        toast.error(`モード切替に失敗: ${(e as Error).message}`)
      } finally {
        setOptimistic(null)
      }
    },
    [optimistic, state?.mode, refetch],
  )

  const saveSettings = useCallback(
    async (patch: Partial<ModeSettings>) => {
      try {
        const r = await api<ModeState>("/mode/settings", { method: "PUT", json: patch })
        setOverride(r)
        toast.success("保存しました", {
          description: r.restarted?.length ? `${r.restarted.length} 台のサーバを新しい設定で再起動` : undefined,
        })
        refetch()
        return true
      } catch (e) {
        toast.error((e as Error).message)
        return false
      }
    },
    [refetch],
  )

  return (
    <ModeContext.Provider value={{ state, mode: optimistic ?? state?.mode, pending: optimistic !== null, setMode, saveSettings, refetch }}>
      {children}
    </ModeContext.Provider>
  )
}

// eslint-disable-next-line react-refresh/only-export-components
export const useMode = () => useContext(ModeContext)

/** Short one-line summary of the current limits, e.g. "CPU 12/24 コア · VRAM 4 GB 予約". */
// eslint-disable-next-line react-refresh/only-export-components
export function profileSummary(s: ModeState): string {
  const p = s.profile
  const parts = [`CPU ${p.physical_cores}/${s.topology.physical_cores} コア`]
  if (p.mode === "shared") parts.push(`VRAM ${gb(p.vram_reserve_mb)} GB 予約`)
  if (p.gpu_power_percent < 100) parts.push(`GPU 電力 ${p.gpu_power_percent}%`)
  return parts.join(" · ")
}

export function gb(mb: number): string {
  const v = mb / 1024
  return Number.isInteger(v) ? String(v) : v.toFixed(1)
}

export const MODE_STYLE: Record<ModeName, { text: string; bg: string; border: string; active: string }> = {
  full: {
    text: "text-emerald-600 dark:text-emerald-400",
    bg: "bg-emerald-500/10",
    border: "border-emerald-500/40",
    active: "bg-emerald-500 text-white shadow-sm",
  },
  shared: {
    text: "text-amber-600 dark:text-amber-400",
    bg: "bg-amber-500/10",
    border: "border-amber-500/40",
    active: "bg-amber-500 text-white shadow-sm",
  },
}
