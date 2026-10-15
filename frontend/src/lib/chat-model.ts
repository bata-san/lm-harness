// Shapes of the encrypted chat vault items and the pure helpers around them.
// Nothing here touches the network or the vault; see lib/vault.ts for storage.

export type ChatParams = { temperature?: number; top_p?: number; max_tokens?: number }
export type ChatUsage = { prompt_tokens?: number; completion_tokens?: number; total_tokens?: number }

export type ChatMessage = {
  id: string
  role: "user" | "assistant"
  content: string
  reasoning?: string
  created_at: number
  model?: string
  usage?: ChatUsage
  /** Completion tokens per second, measured from the first streamed token. */
  tps?: number
}

/** Plain metadata, visible only after decryption of the head. */
export type ChatHead = {
  title: string
  pinned: boolean
  persona_id: string | null
  model: string | null
  updated: number
}

export type ChatBody = {
  system: string
  params: ChatParams
  messages: ChatMessage[]
}

export type PersonaHead = { name: string; emoji: string; builtin?: boolean }
export type PersonaBody = { description: string; system: string; model?: string | null; params: ChatParams }

export type WireMessage = { role: "system" | "user" | "assistant"; content: string }

export const DEFAULT_TITLE = "新しいチャット"
const TITLE_CHARS = 30

/** RFC 4122 v4 UUID (lowercase), built from getRandomValues so it works without randomUUID(). */
export function newId(): string {
  const b = crypto.getRandomValues(new Uint8Array(16))
  b[6] = (b[6] & 0x0f) | 0x40
  b[8] = (b[8] & 0x3f) | 0x80
  const h = [...b].map((x) => x.toString(16).padStart(2, "0")).join("")
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`
}

/** Chat title derived from the first user message. */
export function titleFrom(text: string): string {
  const t = text.trim().replace(/\s+/g, " ")
  if (!t) return DEFAULT_TITLE
  const chars = [...t]
  return chars.length > TITLE_CHARS ? `${chars.slice(0, TITLE_CHARS).join("")}…` : t
}

/** Messages as sent to the model: system prompt first, reasoning and metadata stripped. */
export function toWire(system: string, messages: ChatMessage[]): WireMessage[] {
  const out: WireMessage[] = []
  if (system.trim()) out.push({ role: "system", content: system })
  for (const m of messages) out.push({ role: m.role, content: m.content })
  return out
}

export function paramsFromInput(p: ChatParams): ChatParams {
  const out: ChatParams = {}
  if (p.temperature !== undefined) out.temperature = p.temperature
  if (p.top_p !== undefined) out.top_p = p.top_p
  if (p.max_tokens !== undefined) out.max_tokens = p.max_tokens
  return out
}

export const DEFAULT_PERSONAS: { head: PersonaHead; body: PersonaBody }[] = [
  {
    head: { name: "汎用アシスタント", emoji: "🤖", builtin: true },
    body: {
      description: "日常の質問や相談に答えます。",
      system: "あなたは親切で正確な日本語のアシスタントです。分からないことは分からないと伝えてください。",
      params: {},
    },
  },
  {
    head: { name: "プログラマ", emoji: "💻", builtin: true },
    body: {
      description: "コードの作成・レビュー・デバッグを手伝います。",
      system:
        "あなたは経験豊富なソフトウェアエンジニアです。コードは実行可能な最小限の例で示し、前提と注意点を簡潔に説明してください。",
      params: { temperature: 0.2 },
    },
  },
  {
    head: { name: "翻訳 (日英)", emoji: "🌐", builtin: true },
    body: {
      description: "日本語は英語に、英語は日本語に訳します。",
      system: "入力が日本語なら英語に、英語なら日本語に翻訳してください。訳文のみを出力し、解説は付けないでください。",
      params: { temperature: 0.3 },
    },
  },
  {
    head: { name: "要約", emoji: "📝", builtin: true },
    body: {
      description: "長い文章の要点を箇条書きにまとめます。",
      system: "与えられた文章の要点を箇条書きで簡潔にまとめてください。文章と同じ言語で出力してください。",
      params: {},
    },
  },
]

function safeFilename(name: string): string {
  const cleaned = name.replace(/[\\/:*?"<>|\n\r\t]/g, "_").trim()
  return (cleaned || "chat").slice(0, 60)
}

export function exportFilename(title: string, ext: "md" | "json"): string {
  return `${safeFilename(title)}.${ext}`
}

const ROLE_LABEL: Record<ChatMessage["role"], string> = { user: "あなた", assistant: "アシスタント" }

/** Human-readable transcript. Reasoning is quoted, not rendered as HTML. */
export function exportMarkdown(head: ChatHead, body: ChatBody): string {
  const lines: string[] = [`# ${head.title}`, "", `- モデル: ${head.model ?? "不明"}`, `- 更新: ${new Date(head.updated).toLocaleString("ja-JP")}`]
  if (body.system.trim()) lines.push("", "## システムプロンプト", "", body.system)
  for (const m of body.messages) {
    lines.push("", `## ${ROLE_LABEL[m.role]}${m.model ? ` (${m.model})` : ""}`)
    if (m.reasoning) lines.push("", ...m.reasoning.split("\n").map((l) => `> ${l}`))
    lines.push("", m.content)
  }
  return lines.join("\n") + "\n"
}

export function exportJson(head: ChatHead, body: ChatBody): string {
  return JSON.stringify({ title: head.title, model: head.model, updated: head.updated, ...body }, null, 2) + "\n"
}

/** Save text as a file via a temporary object URL. */
export function downloadText(filename: string, text: string, mime: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: `${mime};charset=utf-8` }))
  const a = document.createElement("a")
  a.href = url
  a.download = filename
  document.body.appendChild(a)
  a.click()
  a.remove()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
