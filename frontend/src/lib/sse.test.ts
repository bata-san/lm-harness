import { describe, expect, it } from "vitest"

import { createSseParser } from "./sse"

describe("createSseParser", () => {
  it("handles events split across arbitrary chunk boundaries", () => {
    const out: string[] = []
    const p = createSseParser((d) => out.push(d))
    const wire = 'data: {"a":1}\n\ndata: {"b":"x\\ny"}\n\ndata: [DONE]\n\n'
    for (const ch of wire) p.push(ch)
    p.end()
    expect(out).toEqual(['{"a":1}', '{"b":"x\\ny"}', "[DONE]"])
  })

  it("accepts CRLF, skips comments, and joins multi-line data", () => {
    const out: string[] = []
    const p = createSseParser((d) => out.push(d))
    p.push(": keep-alive\r\ndata: one\r\ndata: two\r\n\r\n")
    p.end()
    expect(out).toEqual(["one\ntwo"])
  })

  it("dispatches an unterminated trailing event on end()", () => {
    const out: string[] = []
    const p = createSseParser((d) => out.push(d))
    p.push("data: tail")
    expect(out).toEqual([])
    p.end()
    expect(out).toEqual(["tail"])
  })
})
