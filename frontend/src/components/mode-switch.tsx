import { ArmchairIcon, Loader2Icon, ZapIcon } from "lucide-react"

import { MODE_LABEL, MODE_STYLE, SOURCE_LABEL, useMode } from "@/lib/mode"
import type { ModeName } from "@/lib/api"
import { cn } from "@/lib/utils"

export const MODE_ICON = { full: ZapIcon, shared: ArmchairIcon } as const

/** Two-option segmented control that switches the performance mode. */
export function ModeSwitch() {
  const { mode, state, pending, setMode } = useMode()
  return (
    <div className="flex items-center gap-2">
      <div
        role="radiogroup"
        aria-label="パフォーマンスモード"
        className={cn(
          "inline-flex rounded-lg border p-0.5 transition-colors",
          mode ? MODE_STYLE[mode].border : "border-border",
          mode ? MODE_STYLE[mode].bg : "bg-muted",
        )}
      >
        {(["full", "shared"] as ModeName[]).map((m) => {
          const Icon = MODE_ICON[m]
          const active = mode === m
          return (
            <button
              key={m}
              type="button"
              role="radio"
              aria-checked={active}
              disabled={!state || pending}
              onClick={() => setMode(m)}
              title={m === "full" ? "ワークステーションの全性能を推論に使う" : "人が使えるよう CPU/GPU/VRAM/ダウンロードを制限する"}
              className={cn(
                "inline-flex items-center gap-1.5 rounded-md px-2.5 py-1 text-sm font-medium transition-colors disabled:cursor-wait",
                active ? MODE_STYLE[m].active : "text-muted-foreground hover:text-foreground",
              )}
            >
              {pending && active ? <Loader2Icon className="size-4 animate-spin" /> : <Icon className="size-4" />}
              <span className="hidden sm:inline">{MODE_LABEL[m]}</span>
            </button>
          )
        })}
      </div>
      {state && (
        <span className="hidden text-[11px] leading-tight text-muted-foreground md:block" title={state.switched_at ? new Date(state.switched_at * 1000).toLocaleString() : undefined}>
          {SOURCE_LABEL[state.source] ?? state.source}
        </span>
      )}
    </div>
  )
}
