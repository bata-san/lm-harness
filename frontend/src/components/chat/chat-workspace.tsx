import { useCallback, useEffect, useRef, useState } from "react"
import { MenuIcon, SlidersHorizontalIcon } from "lucide-react"
import { toast } from "sonner"

import { ChatSettingsDialog } from "@/components/chat/chat-settings-dialog"
import { Composer } from "@/components/chat/composer"
import { AutoLockPopover, InfoPopover, LockButton } from "@/components/chat/header"
import { PersonaDialog, type PersonaEntry } from "@/components/chat/persona-dialog"
import { ThreadList, type ThreadEntry } from "@/components/chat/thread-list"
import { Transcript, type LiveTurn } from "@/components/chat/transcript"
import { useVault } from "@/components/chat/vault-context"
import { Button } from "@/components/ui/button"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { ChatHttpError, listModels, streamChat } from "@/lib/chat-api"
import { copyText } from "@/lib/clipboard"
import {
  DEFAULT_PERSONAS,
  DEFAULT_TITLE,
  downloadText,
  exportFilename,
  exportJson,
  exportMarkdown,
  newId,
  titleFrom,
  toWire,
  type ChatBody,
  type ChatHead,
  type ChatMessage,
  type ChatParams,
  type ChatUsage,
  type PersonaBody,
  type PersonaHead,
} from "@/lib/chat-model"
import { deleteItem, loadAll, loadItem, saveEncrypted } from "@/lib/vault"

const LAST_MODEL_KEY = "chat.lastModel"
const PERSONAS_SEEDED_KEY = "chat.personasSeeded"
const MODEL_POLL_MS = 5000

/** The chat currently shown. `saved` is false until the first turn has been stored. */
type Active = { id: string; head: ChatHead; body: ChatBody; saved: boolean }
type Live = LiveTurn & { chatId: string }

function makeDraft(): Active {
  return {
    id: newId(),
    head: { title: DEFAULT_TITLE, pinned: false, persona_id: null, model: null, updated: Date.now() },
    body: { system: "", params: {}, messages: [] },
    saved: false,
  }
}

function readStr(key: string): string | null {
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function writeStr(key: string, value: string): void {
  try {
    localStorage.setItem(key, value)
  } catch {
    /* storage unavailable */
  }
}

function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/**
 * The chat page body. Everything here runs with the vault unlocked (see VaultGate).
 * Turns are persisted (encrypted) after they complete, stop, or fail; drafts live only in state.
 */
export function ChatWorkspace() {
  const session = useVault()
  const [threads, setThreads] = useState<ThreadEntry[]>()
  const [personas, setPersonas] = useState<PersonaEntry[]>([])
  const [models, setModels] = useState<string[]>()
  const [modelError, setModelError] = useState<string | null>(null)
  const [lastModel, setLastModel] = useState<string | null>(() => readStr(LAST_MODEL_KEY))
  const [active, setActive] = useState<Active>(makeDraft)
  const [live, setLive] = useState<Live | null>(null)
  const [turnError, setTurnError] = useState<{ chatId: string; message: string } | null>(null)
  const [draft, setDraft] = useState("")
  const [editing, setEditing] = useState<{ id: string; text: string } | null>(null)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [personasOpen, setPersonasOpen] = useState(false)
  const [drawerOpen, setDrawerOpen] = useState(false)

  const abortRef = useRef<AbortController | null>(null)
  const deletedRef = useRef(new Set<string>())
  const initRef = useRef(false)

  const busy = live !== null
  const listed = models ?? []
  const model =
    (active.head.model && listed.includes(active.head.model) ? active.head.model : null) ??
    (lastModel && listed.includes(lastModel) ? lastModel : null) ??
    listed[0] ??
    null
  const shownLive = live && live.chatId === active.id ? live : null
  const shownError = turnError && turnError.chatId === active.id ? turnError.message : null
  const playablePersonas = personas.filter((p): p is Extract<PersonaEntry, { locked: false }> => !p.locked)
  const activePersona = playablePersonas.find((p) => p.id === active.head.persona_id) ?? null

  const refreshThreads = useCallback(async () => {
    try {
      setThreads(await loadAll<ChatHead>(session, "chat"))
    } catch (e) {
      toast.error(`一覧を読み込めません: ${errText(e)}`)
    }
  }, [session])

  const refreshPersonas = useCallback(async () => {
    try {
      setPersonas(await loadAll<PersonaHead>(session, "persona"))
    } catch (e) {
      toast.error(`ペルソナを読み込めません: ${errText(e)}`)
    }
  }, [session])

  // One-time init. The ref guards StrictMode's double effect run so personas are seeded once.
  useEffect(() => {
    if (initRef.current) return
    initRef.current = true
    void (async () => {
      try {
        let list = await loadAll<PersonaHead>(session, "persona")
        if (list.length === 0 && readStr(PERSONAS_SEEDED_KEY) === null) {
          for (const p of DEFAULT_PERSONAS) await saveEncrypted(session, "persona", newId(), p.head, p.body)
          writeStr(PERSONAS_SEEDED_KEY, "1")
          list = await loadAll<PersonaHead>(session, "persona")
        }
        setPersonas(list)
      } catch (e) {
        toast.error(`ペルソナを読み込めません: ${errText(e)}`)
      }
      void refreshThreads()
    })()
  }, [session, refreshThreads])

  const loadModels = useCallback(async () => {
    try {
      setModels(await listModels())
      setModelError(null)
    } catch (e) {
      if (e instanceof ChatHttpError && e.status === 503) {
        // 503 means no instance is running: an empty list, not an error.
        setModels([])
        setModelError(null)
      } else {
        // Keep the last good list on a transient failure; show why it could not be refreshed.
        setModels((prev) => prev ?? [])
        setModelError(errText(e))
      }
    }
  }, [])

  useEffect(() => {
    void loadModels()
    const id = setInterval(() => {
      if (document.visibilityState === "visible") void loadModels()
    }, MODEL_POLL_MS)
    return () => clearInterval(id)
  }, [loadModels])

  // Locking unmounts this component: stop any reply in flight (its partial text is not saved once locked).
  useEffect(() => () => abortRef.current?.abort(), [])

  const persist = async (id: string, head: ChatHead, body: ChatBody): Promise<boolean> => {
    if (!session.isUnlocked || deletedRef.current.has(id)) return false
    try {
      await saveEncrypted(session, "chat", id, head, body)
      return true
    } catch (e) {
      toast.error(`保存できませんでした: ${errText(e)}`)
      return false
    }
  }

  /** Stream one assistant reply to `chat` (whose transcript must end with a user message). */
  const runTurn = async (chat: Active, use: string) => {
    const ctrl = new AbortController()
    abortRef.current = ctrl
    const turn: { content: string; reasoning: string; firstAt: number | null; usage?: ChatUsage } = { content: "", reasoning: "", firstAt: null }
    setTurnError(null)
    setLive({ chatId: chat.id, content: "", reasoning: "", firstAt: null })

    let frame = 0
    const push = () => {
      frame = 0
      setLive({ chatId: chat.id, content: turn.content, reasoning: turn.reasoning, firstAt: turn.firstAt })
    }
    const schedule = () => {
      session.touch()
      if (turn.firstAt === null) turn.firstAt = Date.now()
      if (!frame) frame = requestAnimationFrame(push)
    }

    let failure: string | null = null
    try {
      await streamChat(
        { model: use, messages: toWire(chat.body.system, chat.body.messages), params: chat.body.params, signal: ctrl.signal },
        {
          onContent: (t) => {
            turn.content += t
            schedule()
          },
          onReasoning: (t) => {
            turn.reasoning += t
            schedule()
          },
          onUsage: (u) => {
            turn.usage = u
          },
        },
      )
    } catch (e) {
      // An abort is a user stop (or a lock / chat switch): keep what arrived.
      if (!ctrl.signal.aborted) failure = errText(e)
    } finally {
      if (frame) cancelAnimationFrame(frame)
      if (abortRef.current === ctrl) abortRef.current = null
    }

    const now = Date.now()
    const hasText = turn.content.length > 0 || turn.reasoning.length > 0
    let messages = chat.body.messages
    if (hasText) {
      const completion = turn.usage?.completion_tokens
      const secs = turn.firstAt ? (now - turn.firstAt) / 1000 : 0
      const tps = completion !== undefined && secs > 0.2 ? Math.round((completion / secs) * 10) / 10 : undefined
      const reply: ChatMessage = {
        id: newId(),
        role: "assistant",
        content: turn.content,
        reasoning: turn.reasoning || undefined,
        created_at: now,
        model: use,
        usage: turn.usage,
        tps,
      }
      messages = [...messages, reply]
    }

    const head: ChatHead = { ...chat.head, model: use, updated: now }
    const body: ChatBody = { ...chat.body, messages }
    setActive((prev) => (prev.id === chat.id ? { ...prev, head, body } : prev))
    setLive((prev) => (prev?.chatId === chat.id ? null : prev))
    if (failure) setTurnError({ chatId: chat.id, message: failure })

    // Completed, stopped and failed turns are all stored; a failed one keeps the user's message.
    const ok = await persist(chat.id, head, body)
    if (ok) {
      setActive((prev) => (prev.id === chat.id ? { ...prev, saved: true } : prev))
      void refreshThreads()
    }
  }

  /** Apply a change to the active chat and store it if it has been saved before. */
  const commit = async (next: Active) => {
    setActive(next)
    if (next.saved && (await persist(next.id, next.head, next.body))) void refreshThreads()
  }

  const chooseModel = (m: string) => {
    setLastModel(m)
    writeStr(LAST_MODEL_KEY, m)
    void commit({ ...active, head: { ...active.head, model: m } })
  }

  const send = () => {
    const text = draft.trim()
    if (!text || !model || busy) return
    const now = Date.now()
    const userMsg: ChatMessage = { id: newId(), role: "user", content: text, created_at: now }
    const messages = [...active.body.messages, userMsg]
    const next: Active = {
      ...active,
      head: {
        ...active.head,
        title: active.body.messages.length === 0 && active.head.title === DEFAULT_TITLE ? titleFrom(text) : active.head.title,
        updated: now,
      },
      body: { ...active.body, messages },
    }
    setDraft("")
    setLastModel(model)
    writeStr(LAST_MODEL_KEY, model)
    setActive(next)
    void runTurn(next, model)
  }

  const stop = () => abortRef.current?.abort()

  const retry = () => {
    if (!model || busy) return
    setTurnError(null)
    void runTurn(active, model)
  }

  const regenerate = () => {
    const msgs = active.body.messages
    if (!model || busy || msgs[msgs.length - 1]?.role !== "assistant") return
    const next: Active = { ...active, body: { ...active.body, messages: msgs.slice(0, -1) } }
    setActive(next)
    void runTurn(next, model)
  }

  const submitEdit = () => {
    if (!editing || !model || busy) return
    const text = editing.text.trim()
    const idx = active.body.messages.findIndex((m) => m.id === editing.id)
    if (!text || idx < 0) return
    const kept = [...active.body.messages.slice(0, idx), { ...active.body.messages[idx], content: text, created_at: Date.now() }]
    const next: Active = { ...active, body: { ...active.body, messages: kept }, head: { ...active.head, updated: Date.now() } }
    setEditing(null)
    setActive(next)
    void runTurn(next, model)
  }

  /** Current contents of a chat: the in-memory copy if it is the active one, else decrypted from the server. */
  const recordOf = async (id: string): Promise<{ head: ChatHead; body: ChatBody }> => {
    if (active.id === id) return { head: active.head, body: active.body }
    const rec = await loadItem<ChatHead, ChatBody>(session, "chat", id)
    return { head: rec.head, body: rec.body }
  }

  const updateHead = async (id: string, patch: Partial<ChatHead>) => {
    try {
      const cur = await recordOf(id)
      const head = { ...cur.head, ...patch }
      if (active.id === id) setActive((prev) => (prev.id === id ? { ...prev, head } : prev))
      await saveEncrypted(session, "chat", id, head, cur.body)
      void refreshThreads()
    } catch (e) {
      toast.error(`更新できませんでした: ${errText(e)}`)
    }
  }

  const openChat = async (id: string) => {
    setDrawerOpen(false)
    if (id === active.id) return
    abortRef.current?.abort()
    setEditing(null)
    try {
      const rec = await loadItem<ChatHead, ChatBody>(session, "chat", id)
      setActive({ id: rec.id, head: rec.head, body: rec.body, saved: true })
    } catch (e) {
      toast.error(`開けませんでした: ${errText(e)}`)
    }
  }

  const newChat = () => {
    setDrawerOpen(false)
    abortRef.current?.abort()
    setEditing(null)
    setActive(makeDraft())
  }

  const deleteChat = async (id: string) => {
    deletedRef.current.add(id)
    if (active.id === id) abortRef.current?.abort()
    await deleteItem("chat", id)
    if (active.id === id) {
      setActive(makeDraft())
      setTurnError(null)
      setEditing(null)
    }
    void refreshThreads()
  }

  const exportChat = async (id: string, format: "md" | "json") => {
    try {
      const cur = await recordOf(id)
      const text = format === "md" ? exportMarkdown(cur.head, cur.body) : exportJson(cur.head, cur.body)
      downloadText(exportFilename(cur.head.title, format), text, format === "md" ? "text/markdown" : "application/json")
    } catch (e) {
      toast.error(`書き出せませんでした: ${errText(e)}`)
    }
  }

  const applyPersona = async (pid: string | null) => {
    if (pid === null) {
      await commit({ ...active, head: { ...active.head, persona_id: null } })
      return
    }
    try {
      const rec = await loadItem<PersonaHead, PersonaBody>(session, "persona", pid)
      const personaModel = rec.body.model && listed.includes(rec.body.model) ? rec.body.model : active.head.model
      await commit({
        ...active,
        head: { ...active.head, persona_id: pid, model: personaModel },
        body: { ...active.body, system: rec.body.system, params: { ...rec.body.params } },
      })
    } catch (e) {
      toast.error(`ペルソナを適用できません: ${errText(e)}`)
    }
  }

  const saveSettings = async (system: string, params: ChatParams) => {
    await commit({ ...active, body: { ...active.body, system, params } })
  }

  const list = (
    <ThreadList
      threads={threads}
      activeId={active.id}
      draft={active.saved ? null : { id: active.id, title: active.head.title }}
      onOpen={(id) => void openChat(id)}
      onNew={newChat}
      onRename={async (id, title) => updateHead(id, { title })}
      onTogglePin={async (id, pinned) => updateHead(id, { pinned })}
      onDelete={deleteChat}
      onExport={exportChat}
    />
  )

  return (
    <div className="flex h-[calc(100svh-5rem)] min-h-[32rem] overflow-hidden rounded-xl border bg-background sm:h-[calc(100svh-5.5rem)] md:h-[calc(100svh-6.5rem)]">
      <aside className="hidden w-72 shrink-0 border-r p-3 md:flex md:flex-col">{list}</aside>

      <section className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-12 shrink-0 items-center gap-1 border-b px-2 sm:px-3">
          <Button variant="ghost" size="icon-sm" className="md:hidden" aria-label="チャット一覧を開く" onClick={() => setDrawerOpen(true)}>
            <MenuIcon />
          </Button>
          <h2 className="min-w-0 flex-1 truncate px-1 text-sm font-medium">{active.head.title}</h2>
          <Button variant="ghost" size="icon-sm" aria-label="チャットの設定" title="システムプロンプトと生成パラメータ" disabled={busy} onClick={() => setSettingsOpen(true)}>
            <SlidersHorizontalIcon />
          </Button>
          <AutoLockPopover />
          <InfoPopover />
          <LockButton />
        </header>

        <Transcript
          messages={active.body.messages}
          live={shownLive}
          error={shownError}
          busy={busy}
          editing={editing}
          onEditText={(text) => setEditing((e) => (e ? { ...e, text } : e))}
          onEditStart={(id) => {
            const m = active.body.messages.find((x) => x.id === id)
            if (m) setEditing({ id, text: m.content })
          }}
          onEditCancel={() => setEditing(null)}
          onEditSubmit={submitEdit}
          onCopy={(t) => void copyText(t)}
          onRegenerate={regenerate}
          onRetry={retry}
          emptyTitle={activePersona ? `${activePersona.head.emoji} ${activePersona.head.name}` : "何を話しましょうか?"}
        />

        <Composer
          value={draft}
          onChange={setDraft}
          onSend={send}
          onStop={stop}
          busy={busy}
          models={models}
          modelError={modelError}
          model={model}
          onModel={chooseModel}
          personas={playablePersonas.map((p) => ({ id: p.id, label: `${p.head.emoji} ${p.head.name}` }))}
          personaId={active.head.persona_id}
          onPersona={(id) => void applyPersona(id)}
          onManagePersonas={() => setPersonasOpen(true)}
        />
      </section>

      <Sheet open={drawerOpen} onOpenChange={setDrawerOpen}>
        <SheetContent side="left" className="flex w-[85vw] max-w-xs flex-col gap-3 p-4">
          <SheetHeader className="p-0">
            <SheetTitle>チャット一覧</SheetTitle>
            <SheetDescription>会話を選ぶか、新しく始めます。</SheetDescription>
          </SheetHeader>
          <div className="min-h-0 flex-1">{list}</div>
        </SheetContent>
      </Sheet>

      {settingsOpen && (
        <ChatSettingsDialog
          open
          onOpenChange={(o) => !o && setSettingsOpen(false)}
          system={active.body.system}
          params={active.body.params}
          onSave={(system, params) => saveSettings(system, params)}
        />
      )}

      <PersonaDialog
        open={personasOpen}
        onOpenChange={setPersonasOpen}
        personas={personas}
        models={listed}
        onChanged={refreshPersonas}
      />
    </div>
  )
}
