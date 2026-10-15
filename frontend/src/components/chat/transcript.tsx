import { useEffect, useRef } from "react"
import { CopyIcon, PencilLineIcon, RefreshCwIcon, RotateCcwIcon, ShieldAlertIcon } from "lucide-react"

import { Markdown } from "@/components/chat/markdown"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Textarea } from "@/components/ui/textarea"
import type { ChatMessage } from "@/lib/chat-model"
import { cn } from "@/lib/utils"

export type LiveTurn = { content: string; reasoning: string; firstAt: number | null }

type Props = {
  messages: ChatMessage[]
  live: LiveTurn | null
  error: string | null
  busy: boolean
  editing: { id: string; text: string } | null
  onEditText: (text: string) => void
  onEditStart: (id: string) => void
  onEditCancel: () => void
  onEditSubmit: () => void
  onCopy: (text: string) => void
  onRegenerate: () => void
  onRetry: () => void
  emptyTitle: string
}

/** Scrolling transcript. Follows new output only while the reader is near the bottom. */
export function Transcript(p: Props) {
  const scroller = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  const lastUser = [...p.messages].reverse().find((m) => m.role === "user")
  const lastMessage = p.messages[p.messages.length - 1]

  useEffect(() => {
    const el = scroller.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [p.messages.length, p.live?.content, p.live?.reasoning, p.editing?.id, p.error])

  return (
    <div
      ref={scroller}
      className="min-h-0 flex-1 overflow-y-auto"
      onScroll={(e) => {
        const el = e.currentTarget
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80
      }}
    >
      <div className="mx-auto grid max-w-3xl gap-6 px-4 py-6">
        {p.messages.length === 0 && !p.live && (
          <div className="flex flex-col items-center gap-2 py-16 text-center text-muted-foreground">
            <p className="text-lg font-medium text-foreground">{p.emptyTitle}</p>
            <p className="text-sm">下の入力欄からメッセージを送信してください。Enter で送信、Shift+Enter で改行します。</p>
          </div>
        )}

        {p.messages.map((m) => {
          const isEditing = p.editing?.id === m.id
          if (m.role === "user") {
            return (
              <div key={m.id} className="group grid justify-items-end gap-1">
                {isEditing ? (
                  <div className="grid w-full max-w-[85%] gap-2">
                    <Textarea autoFocus rows={3} value={p.editing?.text ?? ""} onChange={(e) => p.onEditText(e.target.value)} aria-label="メッセージを編集" />
                    <div className="flex justify-end gap-2">
                      <Button variant="outline" size="sm" onClick={p.onEditCancel}>
                        キャンセル
                      </Button>
                      <Button size="sm" onClick={p.onEditSubmit} disabled={!p.editing?.text.trim()}>
                        保存して再生成
                      </Button>
                    </div>
                  </div>
                ) : (
                  <>
                    <div className="max-w-[85%] rounded-2xl bg-muted px-4 py-2.5 text-[15px] leading-7 break-words whitespace-pre-wrap">{m.content}</div>
                    <div className="flex gap-0.5 opacity-100 transition-opacity md:opacity-0 md:group-hover:opacity-100 md:focus-within:opacity-100">
                      <IconAction label="コピー" onClick={() => p.onCopy(m.content)} icon={<CopyIcon />} />
                      {m.id === lastUser?.id && !p.busy && <IconAction label="編集して再生成" onClick={() => p.onEditStart(m.id)} icon={<PencilLineIcon />} />}
                    </div>
                  </>
                )}
              </div>
            )
          }
          return (
            <div key={m.id} className="group grid gap-1">
              {m.reasoning && (
                <details className="rounded-lg border bg-muted/40 px-3 py-2 text-sm">
                  <summary className="cursor-pointer text-muted-foreground select-none">思考過程</summary>
                  <div className="mt-2 text-muted-foreground">
                    <Markdown text={m.reasoning} className="text-sm leading-6" />
                  </div>
                </details>
              )}
              <Markdown text={m.content} />
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                <div className="flex gap-0.5 transition-opacity md:opacity-0 md:group-hover:opacity-100 md:focus-within:opacity-100">
                  <IconAction label="コピー" onClick={() => p.onCopy(m.content)} icon={<CopyIcon />} />
                  {m.id === lastMessage?.id && !p.busy && <IconAction label="再生成" onClick={p.onRegenerate} icon={<RefreshCwIcon />} />}
                </div>
                <Stats m={m} />
              </div>
            </div>
          )
        })}

        {p.live && (
          <div className="grid gap-1" aria-live="polite">
            {p.live.reasoning && (
              <details open className="rounded-lg border bg-muted/40 px-3 py-2 text-sm">
                <summary className="cursor-pointer text-muted-foreground select-none">思考中…</summary>
                <div className="mt-2 text-muted-foreground">
                  <Markdown text={p.live.reasoning} className="text-sm leading-6" />
                </div>
              </details>
            )}
            {p.live.content ? (
              <Markdown text={p.live.content} />
            ) : (
              !p.live.reasoning && <span className="inline-flex gap-1 py-2 text-muted-foreground" aria-label="生成中">
                <span className="size-1.5 animate-bounce rounded-full bg-current [animation-delay:-0.3s]" />
                <span className="size-1.5 animate-bounce rounded-full bg-current [animation-delay:-0.15s]" />
                <span className="size-1.5 animate-bounce rounded-full bg-current" />
              </span>
            )}
            <p className="text-xs text-muted-foreground">生成中… 「停止」で途中まで保存されます</p>
          </div>
        )}

        {p.error && (
          <Alert variant="destructive">
            <ShieldAlertIcon />
            <AlertTitle>応答を取得できませんでした</AlertTitle>
            <AlertDescription>
              <p>{p.error}</p>
              {!p.busy && (
                <Button size="sm" variant="outline" className="mt-2" onClick={p.onRetry}>
                  <RotateCcwIcon /> 再試行
                </Button>
              )}
            </AlertDescription>
          </Alert>
        )}
      </div>
    </div>
  )
}

function IconAction({ label, onClick, icon }: { label: string; onClick: () => void; icon: React.ReactNode }) {
  return (
    <Button variant="ghost" size="icon-xs" aria-label={label} title={label} onClick={onClick} className="text-muted-foreground">
      {icon}
    </Button>
  )
}

function Stats({ m }: { m: ChatMessage }) {
  const parts: string[] = []
  if (m.tps !== undefined) parts.push(`${m.tps.toFixed(1)} tok/s`)
  if (m.usage?.completion_tokens !== undefined) parts.push(`出力 ${m.usage.completion_tokens.toLocaleString("ja-JP")} トークン`)
  if (m.usage?.prompt_tokens !== undefined) parts.push(`入力 ${m.usage.prompt_tokens.toLocaleString("ja-JP")}`)
  if (m.model) parts.push(m.model)
  if (parts.length === 0) return null
  return <span className={cn("text-xs text-muted-foreground tabular-nums")}>{parts.join(" · ")}</span>
}
