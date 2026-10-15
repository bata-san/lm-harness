import { SendHorizontalIcon, SettingsIcon, SquareIcon } from "lucide-react"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Textarea } from "@/components/ui/textarea"

const NONE = "__none__"

type Props = {
  value: string
  onChange: (v: string) => void
  onSend: () => void
  onStop: () => void
  busy: boolean
  /** undefined while the list is loading; [] means nothing is running. */
  models: string[] | undefined
  modelError: string | null
  model: string | null
  onModel: (m: string) => void
  personas: { id: string; label: string }[]
  personaId: string | null
  onPersona: (id: string | null) => void
  onManagePersonas: () => void
}

/** Message input with model and persona selectors. Enter sends, Shift+Enter inserts a newline. */
export function Composer(p: Props) {
  const noModel = p.models !== undefined && p.models.length === 0
  const canSend = !p.busy && p.value.trim().length > 0 && !!p.model
  const modelItems = Object.fromEntries((p.models ?? []).map((m) => [m, m]))
  const personaItems: Record<string, string> = { [NONE]: "ペルソナなし", ...Object.fromEntries(p.personas.map((x) => [x.id, x.label])) }

  return (
    <div className="border-t bg-background px-3 pt-2 pb-3 sm:px-4">
      <div className="mx-auto grid max-w-3xl gap-2">
        {noModel && (
          <Alert>
            <AlertTitle>起動中のモデルがありません</AlertTitle>
            <AlertDescription>
              <a href="#servers" className="text-primary underline underline-offset-4">
                サーバ画面でモデルを起動する
              </a>
              と、ここから会話できます。
              {p.modelError && <span className="mt-1 block text-destructive">モデル一覧を取得できません: {p.modelError}</span>}
            </AlertDescription>
          </Alert>
        )}
        <div className="rounded-2xl border bg-card shadow-xs transition-shadow focus-within:shadow-sm">
          <Textarea
            value={p.value}
            onChange={(e) => p.onChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault()
                if (canSend) p.onSend()
              }
            }}
            placeholder={noModel ? "モデルを起動すると入力できます" : "メッセージを入力"}
            aria-label="メッセージ"
            rows={1}
            disabled={noModel}
            className="max-h-48 min-h-11 resize-none overflow-y-auto border-0 bg-transparent px-4 pt-3 pb-1 text-[15px] shadow-none focus-visible:border-0 focus-visible:ring-0 dark:bg-transparent"
          />
          <div className="flex flex-wrap items-center gap-2 px-2 pb-2">
            <Select value={p.model ?? ""} onValueChange={(v) => v && p.onModel(v)} items={modelItems} disabled={noModel || p.busy}>
              <SelectTrigger size="sm" className="h-7 w-auto max-w-[11rem] min-w-0 gap-1 border-transparent bg-transparent px-2 text-xs shadow-none hover:bg-muted" aria-label="モデル">
                <SelectValue placeholder="モデル" />
              </SelectTrigger>
              <SelectContent>
                {(p.models ?? []).map((m) => (
                  <SelectItem key={m} value={m}>
                    {m}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>

            <Select value={p.personaId ?? NONE} onValueChange={(v) => p.onPersona(v === NONE ? null : v)} items={personaItems} disabled={p.busy}>
              <SelectTrigger size="sm" className="h-7 w-auto max-w-[10rem] min-w-0 gap-1 border-transparent bg-transparent px-2 text-xs shadow-none hover:bg-muted" aria-label="ペルソナ">
                <SelectValue placeholder="ペルソナ" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={NONE}>ペルソナなし</SelectItem>
                {p.personas.map((x) => (
                  <SelectItem key={x.id} value={x.id}>
                    {x.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant="ghost" size="icon-xs" onClick={p.onManagePersonas} aria-label="ペルソナを管理" title="ペルソナを管理" className="text-muted-foreground">
              <SettingsIcon />
            </Button>

            <div className="ml-auto">
              {p.busy ? (
                <Button size="icon" onClick={p.onStop} aria-label="停止" title="停止" className="rounded-full">
                  <SquareIcon className="fill-current" />
                </Button>
              ) : (
                <Button size="icon" onClick={p.onSend} disabled={!canSend} aria-label="送信" title="送信" className="rounded-full">
                  <SendHorizontalIcon />
                </Button>
              )}
            </div>
          </div>
        </div>
        <p className="text-center text-[11px] text-muted-foreground">AI の回答には誤りが含まれることがあります。</p>
      </div>
    </div>
  )
}
