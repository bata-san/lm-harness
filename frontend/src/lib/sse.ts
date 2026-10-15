// Incremental parser for text/event-stream bodies (OpenAI-style: `data:` lines, a blank line ends an event).

export type SseParser = {
  /** Feed a decoded text chunk; chunks may split lines and events anywhere. */
  push(chunk: string): void
  /** Flush a trailing event that was not terminated by a blank line. */
  end(): void
}

export function createSseParser(onData: (data: string) => void): SseParser {
  let buf = ""
  let data: string[] = []

  const dispatch = () => {
    if (data.length === 0) return
    const payload = data.join("\n")
    data = []
    onData(payload)
  }

  const line = (raw: string) => {
    if (raw === "") return dispatch()
    if (raw.startsWith(":")) return // comment / keep-alive
    const colon = raw.indexOf(":")
    const field = colon === -1 ? raw : raw.slice(0, colon)
    let value = colon === -1 ? "" : raw.slice(colon + 1)
    if (value.startsWith(" ")) value = value.slice(1)
    if (field === "data") data.push(value)
  }

  return {
    push(chunk) {
      buf += chunk
      let nl: number
      while ((nl = buf.indexOf("\n")) !== -1) {
        const raw = buf.slice(0, nl).replace(/\r$/, "")
        buf = buf.slice(nl + 1)
        line(raw)
      }
    },
    end() {
      if (buf.length > 0) {
        line(buf.replace(/\r$/, ""))
        buf = ""
      }
      dispatch()
    },
  }
}
