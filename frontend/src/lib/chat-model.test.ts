import { describe, expect, it } from "vitest"

import { DEFAULT_TITLE, exportMarkdown, newId, titleFrom, toWire, type ChatBody, type ChatHead } from "./chat-model"

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

describe("chat-model helpers", () => {
  it("newId yields lowercase v4 UUIDs accepted by the vault", () => {
    for (let i = 0; i < 20; i++) expect(newId()).toMatch(UUID_V4)
    expect(new Set(Array.from({ length: 50 }, newId)).size).toBe(50)
  })

  it("titleFrom uses the first message, collapses whitespace and truncates", () => {
    expect(titleFrom("  こんにちは\n\n世界  ")).toBe("こんにちは 世界")
    expect(titleFrom("   ")).toBe(DEFAULT_TITLE)
    const long = "あ".repeat(40)
    expect(titleFrom(long)).toBe(`${"あ".repeat(30)}…`)
  })

  it("toWire puts the system prompt first and drops reasoning and metadata", () => {
    const msgs = [{ id: "1", role: "user" as const, content: "q", created_at: 1 }, { id: "2", role: "assistant" as const, content: "a", reasoning: "r", created_at: 2, tps: 9 }]
    expect(toWire("sys", msgs)).toEqual([
      { role: "system", content: "sys" },
      { role: "user", content: "q" },
      { role: "assistant", content: "a" },
    ])
    expect(toWire("  ", msgs)[0].role).toBe("user")
  })

  it("exportMarkdown quotes reasoning and keeps the transcript order", () => {
    const head: ChatHead = { title: "題名", pinned: false, persona_id: null, model: "m1", updated: 0 }
    const body: ChatBody = {
      system: "",
      params: {},
      messages: [
        { id: "1", role: "user", content: "質問", created_at: 1 },
        { id: "2", role: "assistant", content: "回答", reasoning: "一行目\n二行目", created_at: 2, model: "m1" },
      ],
    }
    const md = exportMarkdown(head, body)
    expect(md.startsWith("# 題名\n")).toBe(true)
    expect(md.indexOf("質問")).toBeLessThan(md.indexOf("回答"))
    expect(md).toContain("> 一行目\n> 二行目")
    expect(md).toContain("## アシスタント (m1)")
  })
})
