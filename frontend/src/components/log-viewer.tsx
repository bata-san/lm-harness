import { useEffect, useRef, useState } from "react"

import { api, type LogTail } from "@/lib/api"
import { cn } from "@/lib/utils"

/** Incrementally tails a process log endpoint (…/logs?since=N). */
export function LogViewer({ path, className }: { path: string; className?: string }) {
  const [lines, setLines] = useState<LogTail["lines"]>([])
  const last = useRef(0)
  const box = useRef<HTMLDivElement>(null)
  const stick = useRef(true)

  useEffect(() => {
    last.current = 0
    setLines([])
    let stop = false
    const tick = async () => {
      try {
        const r = await api<LogTail>(`${path}?since=${last.current}`)
        if (stop) return
        if (r.lines.length) {
          last.current = r.last
          setLines((prev) => [...prev, ...r.lines].slice(-3000))
        }
      } catch {
        /* process gone */
      }
    }
    tick()
    const id = setInterval(tick, 1000)
    return () => {
      stop = true
      clearInterval(id)
    }
  }, [path])

  useEffect(() => {
    if (stick.current && box.current) box.current.scrollTop = box.current.scrollHeight
  }, [lines])

  return (
    <div
      ref={box}
      onScroll={(e) => {
        const el = e.currentTarget
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40
      }}
      className={cn("h-96 overflow-auto rounded-lg border bg-zinc-950 p-3 font-mono text-xs leading-relaxed text-zinc-200", className)}
    >
      {lines.length === 0 && <div className="text-zinc-500">ログ待機中…</div>}
      {lines.map((l) => (
        <div
          key={l.seq}
          className={cn(
            "break-all whitespace-pre-wrap",
            l.text.startsWith("$ ") && "text-sky-400",
            l.text.startsWith("[harness]") && "text-emerald-400",
            /error|failed|out of memory/i.test(l.text) && "text-red-400",
          )}
        >
          {l.text}
        </div>
      ))}
    </div>
  )
}
