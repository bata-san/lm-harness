import { afterEach, describe, expect, it, vi } from "vitest"

import { ChatHttpError, listModels, streamChat } from "./chat-api"

function sseBody(pieces: string[]): ReadableStream<Uint8Array> {
  const enc = new TextEncoder()
  return new ReadableStream({
    start(c) {
      for (const p of pieces) c.enqueue(enc.encode(p))
      c.close()
    },
  })
}

function mockFetch(res: Response) {
  const fn = vi.fn(async () => res)
  vi.stubGlobal("fetch", fn)
  return fn
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe("streamChat", () => {
  it("parses reasoning, content and usage across split chunks, stops at [DONE]", async () => {
    const pieces = [
      'data: {"choices":[{"delta":{"reasoning_content":"考え"}}]}\n\nda',
      'ta: {"choices":[{"delta":{"content":"こん"}}]}\n\n',
      'data: {"choices":[{"delta":{"content":"にちは"}}]}\n',
      "\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
      "data: [DONE]\n\n",
      'data: {"choices":[{"delta":{"content":"ignored after DONE"}}]}\n\n',
    ]
    const fetchFn = mockFetch(new Response(sseBody(pieces), { status: 200 }))
    const got = { reasoning: "", content: "", usage: undefined as unknown }
    await streamChat(
      { model: "m", messages: [{ role: "user", content: "hi" }], params: { temperature: 0.5 }, signal: new AbortController().signal },
      {
        onReasoning: (t) => (got.reasoning += t),
        onContent: (t) => (got.content += t),
        onUsage: (u) => (got.usage = u),
      },
    )
    expect(got.reasoning).toBe("考え")
    expect(got.content).toBe("こんにちは")
    expect(got.usage).toEqual({ prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 })

    const [url, init] = fetchFn.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toBe("/v1/chat/completions")
    const sent = JSON.parse(init.body as string)
    expect(sent.stream).toBe(true)
    expect(sent.stream_options).toEqual({ include_usage: true })
    expect(sent.temperature).toBe(0.5)
    expect(sent.top_p).toBeUndefined()
    expect((init.headers as Record<string, string>)["X-Harness-CSRF"]).toBe("1")
  })

  it("maps a 503 to a ChatHttpError with the friendly message", async () => {
    mockFetch(new Response(JSON.stringify({ error: { message: "no model" } }), { status: 503 }))
    const err = await streamChat(
      { model: "m", messages: [], params: {}, signal: new AbortController().signal },
      { onReasoning() {}, onContent() {}, onUsage() {} },
    ).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(ChatHttpError)
    expect((err as ChatHttpError).status).toBe(503)
    expect((err as Error).message).toBe("モデルが起動していません")
  })

  it("surfaces an error event inside the stream", async () => {
    mockFetch(new Response(sseBody(['data: {"error":{"message":"boom"}}\n\n']), { status: 200 }))
    await expect(
      streamChat({ model: "m", messages: [], params: {}, signal: new AbortController().signal }, { onReasoning() {}, onContent() {}, onUsage() {} }),
    ).rejects.toThrow("boom")
  })
})

describe("listModels", () => {
  it("returns the ids from the OpenAI list shape", async () => {
    mockFetch(new Response(JSON.stringify({ object: "list", data: [{ id: "a" }, { id: "b" }, {}] }), { status: 200 }))
    expect(await listModels()).toEqual(["a", "b"])
  })
})
