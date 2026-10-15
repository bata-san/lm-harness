import { useState } from "react"
import { Loader2Icon } from "lucide-react"

import { ParamsFields } from "@/components/chat/params-fields"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Label } from "@/components/ui/label"
import { Textarea } from "@/components/ui/textarea"
import type { ChatParams } from "@/lib/chat-model"

/** Per-chat system prompt and sampling parameters. */
export function ChatSettingsDialog({
  open,
  onOpenChange,
  system: initialSystem,
  params: initialParams,
  onSave,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  system: string
  params: ChatParams
  onSave: (system: string, params: ChatParams) => Promise<void>
}) {
  // Form state is seeded when the dialog opens (the parent unmounts this on close).
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-lg">
        {open && <Form initialSystem={initialSystem} initialParams={initialParams} onSave={onSave} onDone={() => onOpenChange(false)} />}
      </DialogContent>
    </Dialog>
  )
}

function Form({
  initialSystem,
  initialParams,
  onSave,
  onDone,
}: {
  initialSystem: string
  initialParams: ChatParams
  onSave: (system: string, params: ChatParams) => Promise<void>
  onDone: () => void
}) {
  const [system, setSystem] = useState(initialSystem)
  const [params, setParams] = useState<ChatParams>(initialParams)
  const [pending, setPending] = useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (pending) return
    setPending(true)
    try {
      await onSave(system, params)
      onDone()
    } finally {
      setPending(false)
    }
  }

  return (
    <form onSubmit={submit} className="grid gap-4">
      <DialogHeader>
        <DialogTitle>チャットの設定</DialogTitle>
        <DialogDescription>このチャットにだけ適用されます。ペルソナの設定を選ぶと上書きされます。</DialogDescription>
      </DialogHeader>
      <div className="grid gap-1.5">
        <Label htmlFor="chat-system">システムプロンプト</Label>
        <Textarea id="chat-system" rows={5} value={system} onChange={(e) => setSystem(e.target.value)} placeholder="例: あなたは丁寧な日本語で答えるアシスタントです。" />
      </div>
      <div className="grid gap-1.5">
        <Label>生成パラメータ</Label>
        <ParamsFields value={params} onChange={setParams} disabled={pending} />
      </div>
      <DialogFooter>
        <Button type="button" variant="outline" disabled={pending} onClick={onDone}>
          キャンセル
        </Button>
        <Button type="submit" disabled={pending}>
          {pending && <Loader2Icon className="animate-spin" />}
          保存
        </Button>
      </DialogFooter>
    </form>
  )
}
