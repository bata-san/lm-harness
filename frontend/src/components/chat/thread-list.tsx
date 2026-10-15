import { useMemo, useState } from "react"
import { EllipsisIcon, FileJsonIcon, FileTextIcon, LockKeyholeIcon, MessageSquarePlusIcon, Pencil, PinIcon, PinOffIcon, SearchIcon, Trash2Icon } from "lucide-react"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import type { ChatHead } from "@/lib/chat-model"
import { ago } from "@/lib/format"
import { cn } from "@/lib/utils"
import type { VaultListEntry } from "@/lib/vault"

export type ThreadEntry = VaultListEntry<ChatHead>

type Props = {
  threads: ThreadEntry[] | undefined
  activeId: string | null
  /** An unsaved new chat, shown at the top until its first reply is stored. */
  draft: { id: string; title: string } | null
  onOpen: (id: string) => void
  onNew: () => void
  onRename: (id: string, title: string) => Promise<void>
  onTogglePin: (id: string, pinned: boolean) => Promise<void>
  onDelete: (id: string) => Promise<void>
  onExport: (id: string, format: "md" | "json") => Promise<void>
}

/** Thread list: client-side search over decrypted titles, pinned section, locked placeholders. */
export function ThreadList(props: Props) {
  const { threads, activeId, draft, onOpen, onNew, onRename, onTogglePin, onDelete, onExport } = props
  const [query, setQuery] = useState("")
  const [renaming, setRenaming] = useState<{ id: string; title: string } | null>(null)
  const [deleting, setDeleting] = useState<ThreadEntry | null>(null)

  const { pinned, recent, locked } = useMemo(() => {
    const q = query.trim().toLowerCase()
    const open = (threads ?? []).filter((t): t is Extract<ThreadEntry, { locked: false }> => !t.locked)
    const matched = open
      .filter((t) => !q || t.head.title.toLowerCase().includes(q))
      .sort((a, b) => b.head.updated - a.head.updated)
    return {
      pinned: matched.filter((t) => t.head.pinned),
      recent: matched.filter((t) => !t.head.pinned),
      locked: (threads ?? []).filter((t) => t.locked),
    }
  }, [threads, query])

  const showDraft = draft !== null && (!query.trim() || draft.title.toLowerCase().includes(query.trim().toLowerCase()))

  return (
    <div className="flex h-full min-h-0 flex-col gap-3">
      <Button variant="outline" className="w-full justify-start" onClick={onNew}>
        <MessageSquarePlusIcon /> 新しいチャット
      </Button>
      <div className="relative">
        <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="タイトルで検索"
          aria-label="チャットをタイトルで検索"
          className="h-8 pl-8 text-sm"
        />
      </div>

      <nav aria-label="チャット一覧" className="-mx-1 min-h-0 flex-1 overflow-y-auto px-1">
        {threads === undefined && <p className="px-2 py-4 text-sm text-muted-foreground">読み込み中…</p>}
        {showDraft && draft && (
          <Section label="未保存">
            <Row title={draft.title} active={activeId === draft.id} onOpen={() => onOpen(draft.id)} sub="最初の応答後に保存されます" />
          </Section>
        )}
        {pinned.length > 0 && (
          <Section label="固定">
            {pinned.map((t) => (
              <ItemRow key={t.id} entry={t} active={activeId === t.id} onOpen={() => onOpen(t.id)} menu={menuFor(t)} />
            ))}
          </Section>
        )}
        {recent.length > 0 && (
          <Section label="最近">
            {recent.map((t) => (
              <ItemRow key={t.id} entry={t} active={activeId === t.id} onOpen={() => onOpen(t.id)} menu={menuFor(t)} />
            ))}
          </Section>
        )}
        {threads !== undefined && pinned.length === 0 && recent.length === 0 && !showDraft && (
          <p className="px-2 py-4 text-sm text-muted-foreground">{query ? "一致するチャットがありません" : "まだ会話がありません"}</p>
        )}
        {locked.length > 0 && (
          <Section label="復号できない項目">
            {locked.map((t) => (
              <div key={t.id} className="flex items-center gap-1 rounded-lg px-2 py-1.5 text-sm text-muted-foreground">
                <LockKeyholeIcon className="size-3.5 shrink-0" />
                <span className="min-w-0 flex-1 truncate font-mono text-xs">{t.id.slice(0, 8)}…</span>
                <Button variant="ghost" size="icon-xs" aria-label="削除" title="削除" onClick={() => setDeleting(t)}>
                  <Trash2Icon />
                </Button>
              </div>
            ))}
          </Section>
        )}
      </nav>

      {renaming && (
        <RenameDialog
          initial={renaming.title}
          onClose={() => setRenaming(null)}
          onSave={async (title) => {
            await onRename(renaming.id, title)
            setRenaming(null)
          }}
        />
      )}
      {deleting && (
        <ConfirmDialog
          open
          onOpenChange={(o) => !o && setDeleting(null)}
          title="チャットを削除しますか?"
          description={deleting.locked ? "復号できない項目を削除します。" : "この会話はサーバからも完全に削除されます。取り消せません。"}
          confirmLabel="削除"
          onConfirm={async () => {
            await onDelete(deleting.id)
          }}
        />
      )}
    </div>
  )

  function menuFor(t: Extract<ThreadEntry, { locked: false }>) {
    return (
      <>
        <DropdownMenuItem onClick={() => void onTogglePin(t.id, !t.head.pinned)}>
          {t.head.pinned ? <PinOffIcon /> : <PinIcon />} {t.head.pinned ? "固定を外す" : "固定する"}
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => setRenaming({ id: t.id, title: t.head.title })}>
          <Pencil /> 名前を変更
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onClick={() => void onExport(t.id, "md")}>
          <FileTextIcon /> Markdown で書き出し
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => void onExport(t.id, "json")}>
          <FileJsonIcon /> JSON で書き出し
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem variant="destructive" onClick={() => setDeleting(t)}>
          <Trash2Icon /> 削除
        </DropdownMenuItem>
      </>
    )
  }
}

function Section({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="mb-3">
      <p className="px-2 pb-1 text-xs font-medium text-muted-foreground">{label}</p>
      <div className="grid gap-0.5">{children}</div>
    </div>
  )
}

function Row({ title, active, onOpen, sub, menu }: { title: string; active: boolean; onOpen: () => void; sub?: string; menu?: React.ReactNode }) {
  return (
    <div className={cn("group flex items-center gap-1 rounded-lg pr-1 transition-colors hover:bg-muted", active && "bg-muted")}>
      <button type="button" onClick={onOpen} aria-current={active ? "true" : undefined} className="min-w-0 flex-1 px-2 py-1.5 text-left">
        <span className={cn("block truncate text-sm", active && "font-medium")}>{title}</span>
        {sub && <span className="block truncate text-xs text-muted-foreground">{sub}</span>}
      </button>
      {menu}
    </div>
  )
}

function ItemRow({ entry, active, onOpen, menu }: { entry: Extract<ThreadEntry, { locked: false }>; active: boolean; onOpen: () => void; menu: React.ReactNode }) {
  return (
    <Row
      title={entry.head.title}
      active={active}
      onOpen={onOpen}
      sub={ago(entry.head.updated)}
      menu={
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button variant="ghost" size="icon-xs" aria-label="チャットの操作" title="操作" className="shrink-0 opacity-70 hover:opacity-100 focus-visible:opacity-100 aria-expanded:opacity-100 md:opacity-0 md:group-hover:opacity-100" />
            }
          >
            <EllipsisIcon />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end" className="w-52">
            {menu}
          </DropdownMenuContent>
        </DropdownMenu>
      }
    />
  )
}

function RenameDialog({ initial, onSave, onClose }: { initial: string; onSave: (title: string) => Promise<void>; onClose: () => void }) {
  const [title, setTitle] = useState(initial)
  const [pending, setPending] = useState(false)
  const valid = title.trim().length > 0 && !pending

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    if (!valid) return
    setPending(true)
    try {
      await onSave(title.trim())
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog open onOpenChange={(o) => !o && !pending && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>名前を変更</DialogTitle>
            <DialogDescription className="sr-only">チャットの表示名を変更します</DialogDescription>
          </DialogHeader>
          <div className="grid gap-1.5">
            <Label htmlFor="rename-title">タイトル</Label>
            <Input id="rename-title" autoFocus value={title} onChange={(e) => setTitle(e.target.value)} maxLength={120} />
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" disabled={pending} onClick={onClose}>
              キャンセル
            </Button>
            <Button type="submit" disabled={!valid}>
              保存
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
