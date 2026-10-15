import { useEffect, useState } from "react"
import { CheckIcon, Loader2Icon, PlusIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { ParamsFields } from "@/components/chat/params-fields"
import { useVault } from "@/components/chat/vault-context"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Textarea } from "@/components/ui/textarea"
import { newId, type ChatParams, type PersonaBody, type PersonaHead } from "@/lib/chat-model"
import { cn } from "@/lib/utils"
import { deleteItem, loadItem, saveEncrypted, type VaultListEntry } from "@/lib/vault"

export type PersonaEntry = VaultListEntry<PersonaHead>

const NEW = "__new__"

/** Create, edit and delete personas. Editing loads the encrypted body on demand. */
export function PersonaDialog({
  open,
  onOpenChange,
  personas,
  models,
  onChanged,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  personas: PersonaEntry[]
  models: string[]
  onChanged: () => Promise<void>
}) {
  const session = useVault()
  const [selected, setSelected] = useState<string | null>(null)
  const [editor, setEditor] = useState<{ id: string | null; head: PersonaHead; body: PersonaBody } | null>(null)
  const [loadingId, setLoadingId] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<PersonaEntry | null>(null)

  useEffect(() => {
    if (!open) {
      setSelected(null)
      setEditor(null)
    }
  }, [open])

  async function select(entry: PersonaEntry) {
    if (entry.locked) return
    setSelected(entry.id)
    setEditor(null)
    setLoadingId(entry.id)
    try {
      const rec = await loadItem<PersonaHead, PersonaBody>(session, "persona", entry.id)
      setEditor({ id: rec.id, head: rec.head, body: rec.body })
    } catch (e) {
      toast.error(`読み込みに失敗しました: ${(e as Error).message}`)
      setSelected(null)
    } finally {
      setLoadingId(null)
    }
  }

  function create() {
    setSelected(NEW)
    setEditor({ id: null, head: { name: "", emoji: "✨" }, body: { description: "", system: "", model: null, params: {} } })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[90dvh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>ペルソナ</DialogTitle>
          <DialogDescription>ペルソナはシステムプロンプトと生成パラメータの組み合わせです。選んだチャットにだけ適用されます。</DialogDescription>
        </DialogHeader>

        <div className="grid gap-4 sm:grid-cols-[13rem_1fr]">
          <div className="grid content-start gap-1">
            {personas.map((p) => (
              <div key={p.id} className="flex items-center gap-1">
                <button
                  type="button"
                  disabled={p.locked}
                  onClick={() => void select(p)}
                  className={cn(
                    "flex min-w-0 flex-1 items-center gap-2 rounded-lg px-2 py-1.5 text-left text-sm transition-colors hover:bg-muted disabled:opacity-60",
                    selected === p.id && "bg-muted font-medium",
                  )}
                >
                  {p.locked ? (
                    <span className="text-muted-foreground">復号できない項目</span>
                  ) : (
                    <>
                      <span aria-hidden>{p.head.emoji}</span>
                      <span className="truncate">{p.head.name}</span>
                      {loadingId === p.id && <Loader2Icon className="ml-auto size-3.5 animate-spin" />}
                    </>
                  )}
                </button>
                <Button variant="ghost" size="icon-xs" aria-label="削除" title="削除" onClick={() => setPendingDelete(p)}>
                  <Trash2Icon />
                </Button>
              </div>
            ))}
            <Button variant="outline" size="sm" className="mt-2 justify-start" onClick={create}>
              <PlusIcon /> 新規作成
            </Button>
          </div>

          <div className="min-w-0">
            {editor ? (
              <EditorForm key={`${selected}-${editor.id ?? "new"}`} initial={editor} models={models} onChanged={onChanged} onDone={() => { setSelected(null); setEditor(null) }} />
            ) : (
              <p className="py-8 text-center text-sm text-muted-foreground">ペルソナを選ぶか、新規作成してください。</p>
            )}
          </div>
        </div>

        {pendingDelete && (
          <ConfirmDialog
            open
            onOpenChange={(o) => !o && setPendingDelete(null)}
            title="ペルソナを削除しますか?"
            description="この操作は取り消せません。既存のチャットの設定は変わりません。"
            confirmLabel="削除"
            onConfirm={async () => {
              await deleteItem("persona", pendingDelete.id)
              if (selected === pendingDelete.id) {
                setSelected(null)
                setEditor(null)
              }
              await onChanged()
            }}
          />
        )}
      </DialogContent>
    </Dialog>
  )
}

function EditorForm({
  initial,
  models,
  onChanged,
  onDone,
}: {
  initial: { id: string | null; head: PersonaHead; body: PersonaBody }
  models: string[]
  onChanged: () => Promise<void>
  onDone: () => void
}) {
  const session = useVault()
  const [emoji, setEmoji] = useState(initial.head.emoji)
  const [name, setName] = useState(initial.head.name)
  const [description, setDescription] = useState(initial.body.description)
  const [system, setSystem] = useState(initial.body.system)
  const [model, setModel] = useState(initial.body.model ?? "")
  const [params, setParams] = useState<ChatParams>(initial.body.params ?? {})
  const [pending, setPending] = useState(false)

  const valid = name.trim().length > 0 && !pending

  async function save(e: React.FormEvent) {
    e.preventDefault()
    if (!valid) return
    setPending(true)
    try {
      const id = initial.id ?? newId()
      const head: PersonaHead = { name: name.trim(), emoji: emoji.trim() || "✨", ...(initial.head.builtin ? { builtin: true } : {}) }
      const body: PersonaBody = { description: description.trim(), system, model: model || null, params }
      await saveEncrypted(session, "persona", id, head, body)
      await onChanged()
      toast.success("ペルソナを保存しました")
      onDone()
    } catch (err) {
      toast.error(`保存に失敗しました: ${(err as Error).message}`)
    } finally {
      setPending(false)
    }
  }

  return (
    <form onSubmit={save} className="grid gap-4">
      <div className="grid grid-cols-[4.5rem_1fr] gap-3">
        <div className="grid gap-1.5">
          <Label htmlFor="persona-emoji">絵文字</Label>
          <Input id="persona-emoji" maxLength={4} value={emoji} onChange={(e) => setEmoji(e.target.value)} disabled={pending} className="text-center" />
        </div>
        <div className="grid gap-1.5">
          <Label htmlFor="persona-name">名前</Label>
          <Input id="persona-name" value={name} onChange={(e) => setName(e.target.value)} disabled={pending} required />
        </div>
      </div>
      <div className="grid gap-1.5">
        <Label htmlFor="persona-desc">説明</Label>
        <Input id="persona-desc" value={description} onChange={(e) => setDescription(e.target.value)} disabled={pending} />
      </div>
      <div className="grid gap-1.5">
        <Label htmlFor="persona-system">システムプロンプト</Label>
        <Textarea id="persona-system" rows={6} value={system} onChange={(e) => setSystem(e.target.value)} disabled={pending} />
      </div>
      <div className="grid gap-1.5">
        <Label htmlFor="persona-model">既定のモデル</Label>
        <select
          id="persona-model"
          value={model}
          onChange={(e) => setModel(e.target.value)}
          disabled={pending}
          className="h-8 rounded-lg border border-input bg-transparent px-2.5 text-sm outline-none focus-visible:ring-3 focus-visible:ring-ring/50 dark:bg-input/30"
        >
          <option value="">指定しない</option>
          {models.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
          {model && !models.includes(model) && <option value={model}>{model} (未起動)</option>}
        </select>
      </div>
      <div className="grid gap-1.5">
        <Label>生成パラメータ</Label>
        <ParamsFields value={params} onChange={setParams} disabled={pending} />
      </div>
      <DialogFooter>
        <Button type="submit" disabled={!valid}>
          {pending ? <Loader2Icon className="animate-spin" /> : <CheckIcon />}
          保存
        </Button>
      </DialogFooter>
    </form>
  )
}
