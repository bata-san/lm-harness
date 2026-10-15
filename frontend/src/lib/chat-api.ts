// Client for the OpenAI-compatible endpoints served by the harness at the site root (/v1/*).
// These are not under /api, so they do not go through api(); the CSRF header and 401 handling are mirrored here.

import { reportUnauthorized } from "./api"
import { createSseParser } from "./sse"
import type { ChatParams, ChatUsage, WireMessage } from "./chat-model"

export class ChatHttpError extends Error {
  readonly status: number
  constructor(status: number, message: string) {
    super(message)
    this.name = "ChatHttpError"
    this.status = status
  }
}

const CSRF = { "X-Harness-CSRF": "1" }

async function failure(res: Response): Promise<ChatHttpError> {
  if (res.status === 401) reportUnauthorized()
  let detail = ""
  try {
    const body = await res.json()
    if (typeof body?.error?.message === "string") detail = body.error.message
    else if (typeof body?.detail === "string") detail = body.detail
  } catch {
    /* not json */
  }
  const message =
    res.status === 503
      ? "モデルが起動していません"
      : res.status === 401
        ? "ログインが必要です"
        : detail || `${res.status} ${res.statusText}`
  return new ChatHttpError(res.status, message)
}

/** Model ids currently served by the running instances. */
export async function listModels(signal?: AbortSignal): Promise<string[]> {
  const res = await fetch("/v1/models", { credentials: "same-origin", headers: CSRF, signal })
  if (!res.ok) throw await failure(res)
  const body = (await res.json()) as { data?: { id?: unknown }[] }
  return (body.data ?? []).map((m) => m.id).filter((id): id is string => typeof id === "string" && id.length > 0)
}

export type StreamHandlers = {
  onContent: (text: string) => void
  onReasoning: (text: string) => void
  onUsage: (usage: ChatUsage) => void
}

type Chunk = {
  error?: { message?: string }
  usage?: ChatUsage
  choices?: { delta?: { content?: string | null; reasoning_content?: string | null; reasoning?: string | null } }[]
}

/**
 * Stream a chat completion. Resolves when the server sends [DONE] or closes the body.
 * Aborting `signal` rejects with an AbortError (the caller keeps whatever it already received).
 */
export async function streamChat(
  opts: { model: string; messages: WireMessage[]; params: ChatParams; signal: AbortSignal },
  h: StreamHandlers,
): Promise<void> {
  const payload: Record<string, unknown> = {
    model: opts.model,
    messages: opts.messages,
    stream: true,
    stream_options: { include_usage: true },
  }
  for (const k of ["temperature", "top_p", "max_tokens"] as const) {
    if (opts.params[k] !== undefined) payload[k] = opts.params[k]
  }

  const res = await fetch("/v1/chat/completions", {
    method: "POST",
    credentials: "same-origin",
    headers: { ...CSRF, "content-type": "application/json", accept: "text/event-stream" },
    body: JSON.stringify(payload),
    signal: opts.signal,
  })
  if (!res.ok) throw await failure(res)
  if (!res.body) throw new ChatHttpError(res.status, "空の応答を受信しました")

  const reader = res.body.getReader()
  const decoder = new TextDecoder()
  let finished = false
  const parser = createSseParser((data) => {
    if (data === "[DONE]") {
      finished = true
      return
    }
    let chunk: Chunk
    try {
      chunk = JSON.parse(data) as Chunk
    } catch {
      throw new ChatHttpError(500, "ストリームの形式が不正です")
    }
    if (chunk.error) throw new ChatHttpError(500, chunk.error.message ?? "生成中にエラーが発生しました")
    if (chunk.usage) h.onUsage(chunk.usage)
    const delta = chunk.choices?.[0]?.delta
    const reasoning = delta?.reasoning_content ?? delta?.reasoning
    if (reasoning) h.onReasoning(reasoning)
    if (delta?.content) h.onContent(delta.content)
  })

  try {
    for (;;) {
      const { value, done } = await reader.read()
      if (done) break
      parser.push(decoder.decode(value, { stream: true }))
      if (finished) break
    }
    if (!finished) {
      parser.push(decoder.decode())
      parser.end()
    }
  } finally {
    void reader.cancel().catch(() => {})
  }
}
