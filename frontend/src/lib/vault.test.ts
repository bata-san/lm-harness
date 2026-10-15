import { beforeEach, describe, expect, it, vi } from "vitest"
import { api } from "./api"
import {
  PBKDF2_ITERATIONS,
  VaultError,
  VaultSession,
  base64urlDecode,
  base64urlEncode,
  changePassphrase,
  checkVerifier,
  decryptJson,
  deriveKey,
  encryptJson,
  getMeta,
  initVault,
  loadAll,
  loadItem,
  makeVerifier,
  newSalt,
  passphraseStrength,
  saveEncrypted,
} from "./vault"

// Tests use a low iteration count; the production default is asserted separately.
const ITER = 1000

// ---- in-memory fake of the vault HTTP API, injected by mocking ./api ----

type Row = { id: string; updated_at: string; head: string; blob: string }
const store = {
  meta: { v: 1, salt: null as string | null, verifier: null as string | null },
  items: new Map<string, Row>(), // key: `${kind}/${id}`
  puts: 0,
  failAtPut: null as number | null, // throw on exactly this PUT number
}

function resetStore(): void {
  store.meta = { v: 1, salt: null, verifier: null }
  store.items = new Map()
  store.puts = 0
  store.failAtPut = null
}

async function fakeApi(path: string, init?: { method?: string; json?: unknown }): Promise<unknown> {
  const method = init?.method ?? "GET"
  const json = init?.json as Record<string, unknown> | undefined

  if (path === "/vault/meta") {
    if (method === "GET") return { ...store.meta }
    store.meta = { v: 1, salt: json!.salt as string, verifier: json!.verifier as string }
    return undefined
  }
  if (path === "/vault" && method === "DELETE") {
    resetStore()
    return undefined
  }

  const m = /^\/vault\/(chat|persona)(?:\/([^/]+))?$/.exec(path)
  if (!m) throw new Error(`unmodelled route ${method} ${path}`)
  const kind = m[1]
  const id = m[2]
  const prefix = `${kind}/`

  if (!id) {
    if (method !== "GET") throw new Error(`unmodelled ${method} ${path}`)
    return [...store.items.entries()]
      .filter(([k]) => k.startsWith(prefix))
      .map(([, r]) => ({ id: r.id, updated_at: r.updated_at, size: r.blob.length, head: r.head }))
  }

  const k = prefix + id
  if (method === "GET") {
    const rec = store.items.get(k)
    if (!rec) throw new Error("404 Not Found")
    return { ...rec }
  }
  if (method === "PUT") {
    store.puts++
    if (store.failAtPut !== null && store.puts === store.failAtPut) throw new Error("500 simulated")
    store.items.set(k, {
      id,
      updated_at: new Date().toISOString(),
      head: json!.head as string,
      blob: json!.blob as string,
    })
    return undefined
  }
  if (method === "DELETE") {
    store.items.delete(k)
    return undefined
  }
  throw new Error(`unmodelled ${method} ${path}`)
}

vi.mock("./api", () => ({ api: vi.fn() }))

beforeEach(() => {
  resetStore()
  vi.mocked(api).mockImplementation(fakeApi as typeof api)
})

async function freshKey(pass = "correct horse battery"): Promise<{ key: CryptoKey; salt: string }> {
  const salt = newSalt()
  return { key: await deriveKey(pass, salt, ITER), salt }
}

// ---- tests ----

describe("constants", () => {
  it("uses 600000 PBKDF2 iterations by default", () => {
    expect(PBKDF2_ITERATIONS).toBe(600000)
  })
})

describe("base64url", () => {
  it("round-trips arbitrary bytes without padding or std alphabet characters", () => {
    for (let len = 0; len < 40; len++) {
      const bytes = crypto.getRandomValues(new Uint8Array(len))
      const s = base64urlEncode(bytes)
      expect(s).not.toMatch(/[+/=]/)
      expect(Array.from(base64urlDecode(s))).toEqual(Array.from(bytes))
    }
  })

  it("encodes bytes that produce + and / in standard base64 using - and _", () => {
    // 0xfb 0xff encodes to "+/8" in standard base64 and "-_8" in base64url
    expect(base64urlEncode(new Uint8Array([0xfb, 0xff]))).toBe("-_8")
  })

  it("rejects characters outside the base64url alphabet", () => {
    expect(() => base64urlDecode("ab+c")).toThrow(VaultError)
    expect(() => base64urlDecode("abcde")).toThrow(VaultError)
  })

  it("newSalt is 16 bytes of base64url", () => {
    const s = newSalt()
    expect(base64urlDecode(s).length).toBe(16)
    expect(newSalt()).not.toBe(s)
  })
})

describe("encryptJson / decryptJson", () => {
  it("round-trips a JSON value", async () => {
    const { key } = await freshKey()
    const value = { title: "雑談", messages: [{ role: "user", text: "hello" }], n: 3 }
    const blob = await encryptJson(key, "chat", "id-1", value)
    expect(blob.startsWith("v1.")).toBe(true)
    expect(await decryptJson(key, "chat", "id-1", blob)).toEqual(value)
  })

  it("fails with auth-failed under a wrong key", async () => {
    const { key: a } = await freshKey("passphrase-a-long")
    const { key: b } = await freshKey("passphrase-b-long")
    const blob = await encryptJson(a, "chat", "id-1", { x: 1 })
    const err = await decryptJson(b, "chat", "id-1", blob).catch((e) => e)
    expect(err).toBeInstanceOf(VaultError)
    expect((err as VaultError).code).toBe("auth-failed")
  })

  it("fails with auth-failed when one ciphertext byte is flipped", async () => {
    const { key } = await freshKey()
    const blob = await encryptJson(key, "chat", "id-1", { secret: "value" })
    const raw = base64urlDecode(blob.slice(3))
    raw[raw.length - 1] ^= 0x01 // inside the GCM tag
    const tampered = "v1." + base64urlEncode(raw)
    await expect(decryptJson(key, "chat", "id-1", tampered)).rejects.toMatchObject({ code: "auth-failed" })

    const raw2 = base64urlDecode(blob.slice(3))
    raw2[14] ^= 0x80 // inside the ciphertext body
    await expect(decryptJson(key, "chat", "id-1", "v1." + base64urlEncode(raw2))).rejects.toMatchObject({
      code: "auth-failed",
    })
  })

  it("fails when a blob is moved to a different id (AAD binding)", async () => {
    const { key } = await freshKey()
    const blob = await encryptJson(key, "chat", "id-1", { x: 1 })
    await expect(decryptJson(key, "chat", "id-2", blob)).rejects.toMatchObject({ code: "auth-failed" })
  })

  it("fails when a blob is moved to a different kind (AAD binding)", async () => {
    const { key } = await freshKey()
    const blob = await encryptJson(key, "chat", "id-1", { x: 1 })
    await expect(decryptJson(key, "persona", "id-1", blob)).rejects.toMatchObject({ code: "auth-failed" })
  })

  it("uses a fresh random IV on every call", async () => {
    const { key } = await freshKey()
    const a = await encryptJson(key, "chat", "id-1", { same: true })
    const b = await encryptJson(key, "chat", "id-1", { same: true })
    expect(a).not.toBe(b)
    const ivA = base64urlDecode(a.slice(3)).subarray(0, 12)
    const ivB = base64urlDecode(b.slice(3)).subarray(0, 12)
    expect(Array.from(ivA)).not.toEqual(Array.from(ivB))
  })

  it("rejects malformed input as bad-format", async () => {
    const { key } = await freshKey()
    await expect(decryptJson(key, "chat", "id-1", "nope")).rejects.toMatchObject({ code: "bad-format" })
    await expect(decryptJson(key, "chat", "id-1", "v1.!!!")).rejects.toMatchObject({ code: "bad-format" })
    await expect(decryptJson(key, "chat", "id-1", "v1.AAAA")).rejects.toMatchObject({ code: "bad-format" })
  })

  it("derived keys are non-extractable AES-GCM with encrypt and decrypt usages", async () => {
    const { key } = await freshKey()
    expect(key.extractable).toBe(false)
    expect(key.algorithm.name).toBe("AES-GCM")
    expect(Array.from(key.usages).sort()).toEqual(["decrypt", "encrypt"])
  })
})

describe("verifier", () => {
  it("returns true for the right passphrase", async () => {
    const pass = "correct horse battery"
    const { key, salt } = await freshKey(pass)
    const verifier = await makeVerifier(key)
    const again = await deriveKey(pass, salt, ITER)
    expect(await checkVerifier(again, verifier)).toBe(true)
  })

  it("returns false for a wrong passphrase without throwing", async () => {
    const { key, salt } = await freshKey("correct horse battery")
    const verifier = await makeVerifier(key)
    const wrong = await deriveKey("wrong passphrase here", salt, ITER)
    await expect(checkVerifier(wrong, verifier)).resolves.toBe(false)
  })

  it("returns false for a missing or garbage verifier", async () => {
    const { key } = await freshKey()
    await expect(checkVerifier(key, null)).resolves.toBe(false)
    await expect(checkVerifier(key, undefined)).resolves.toBe(false)
    await expect(checkVerifier(key, "garbage")).resolves.toBe(false)
  })
})

describe("passphraseStrength", () => {
  it("scores short passphrases as 0 with a Japanese label", () => {
    expect(passphraseStrength("")).toEqual({ score: 0, label: "短すぎます" })
    expect(passphraseStrength("abc").score).toBe(0)
  })

  it("rises with length and character classes", () => {
    expect(passphraseStrength("abcdefgh").score).toBe(1)
    expect(passphraseStrength("abcdefghijkl").score).toBe(1)
    expect(passphraseStrength("Abcdefghijkl1").score).toBe(2)
    expect(passphraseStrength("Abcdefghijklmnop1").score).toBe(3)
  })
})

describe("VaultSession", () => {
  it("unlocks with the right passphrase and stays locked with the wrong one", async () => {
    const session = new VaultSession()
    await initVault(session, "correct horse battery", ITER)
    expect(session.isUnlocked).toBe(true)
    session.lock()

    const meta = await getMeta()
    await expect(session.unlock("wrong passphrase", meta, ITER)).rejects.toMatchObject({ code: "auth-failed" })
    expect(session.isUnlocked).toBe(false)
    expect(session.key).toBeNull()

    await session.unlock("correct horse battery", meta, ITER)
    expect(session.isUnlocked).toBe(true)
  })

  it("auto-locks after the idle period and touch() postpones it", () => {
    vi.useFakeTimers()
    try {
      const onLock = vi.fn()
      const session = new VaultSession({ idleMinutes: 1, onLock })
      session.rekey({} as CryptoKey)
      vi.advanceTimersByTime(30_000)
      session.touch()
      vi.advanceTimersByTime(45_000)
      expect(session.isUnlocked).toBe(true)
      vi.advanceTimersByTime(20_000)
      expect(session.isUnlocked).toBe(false)
      expect(onLock).toHaveBeenCalledTimes(1)
    } finally {
      vi.useRealTimers()
    }
  })

  it("setIdleMinutes(0) disables auto-lock", () => {
    vi.useFakeTimers()
    try {
      const session = new VaultSession({ idleMinutes: 1 })
      session.setIdleMinutes(0)
      session.rekey({} as CryptoKey)
      vi.advanceTimersByTime(60 * 60_000)
      expect(session.isUnlocked).toBe(true)
    } finally {
      vi.useRealTimers()
    }
  })
})

describe("loadAll", () => {
  it("returns a locked placeholder for an item whose head is corrupted", async () => {
    const session = new VaultSession()
    await initVault(session, "correct horse battery", ITER)
    const goodId = "11111111-1111-4111-8111-111111111111"
    const badId = "22222222-2222-4222-8222-222222222222"
    await saveEncrypted(session, "chat", goodId, { title: "ok" }, { messages: [] })
    await saveEncrypted(session, "chat", badId, { title: "bad" }, { messages: [] })

    const bad = store.items.get(`chat/${badId}`)!
    store.items.set(`chat/${badId}`, { ...bad, head: bad.head.slice(0, -4) + "AAAA" })

    const list = await loadAll<{ title: string }>(session, "chat")
    const byId = Object.fromEntries(list.map((e) => [e.id, e]))
    expect(byId[goodId]).toMatchObject({ locked: false, head: { title: "ok" } })
    expect(byId[badId]).toEqual({ id: badId, locked: true })
  })
})

describe("saveEncrypted / loadItem", () => {
  it("round-trips head and body and rejects a locked session", async () => {
    const session = new VaultSession()
    await initVault(session, "correct horse battery", ITER)
    const id = "33333333-3333-4333-8333-333333333333"
    await saveEncrypted(session, "persona", id, { name: "p" }, { prompt: "secret prompt" })

    const got = await loadItem<{ name: string }, { prompt: string }>(session, "persona", id)
    expect(got.head).toEqual({ name: "p" })
    expect(got.body).toEqual({ prompt: "secret prompt" })

    // The stored data must not contain plaintext.
    const raw = JSON.stringify([...store.items.values()])
    expect(raw).not.toContain("secret prompt")

    session.lock()
    await expect(loadAll(session, "persona")).rejects.toThrow("locked")
  })
})

describe("changePassphrase", () => {
  async function seed(session: VaultSession): Promise<{ chatId: string; personaId: string }> {
    const chatId = "44444444-4444-4444-8444-444444444444"
    const personaId = "55555555-5555-4555-8555-555555555555"
    await saveEncrypted(session, "chat", chatId, { title: "chat" }, { messages: ["hi"] })
    await saveEncrypted(session, "persona", personaId, { name: "persona" }, { prompt: "p" })
    return { chatId, personaId }
  }

  it("re-encrypts every item under the new passphrase", async () => {
    const session = new VaultSession()
    await initVault(session, "old passphrase 1234", ITER)
    const { chatId, personaId } = await seed(session)
    const oldChatBlob = store.items.get(`chat/${chatId}`)!.blob

    await changePassphrase(session, "old passphrase 1234", "new passphrase 5678", ITER)

    expect(store.items.get(`chat/${chatId}`)!.blob).not.toBe(oldChatBlob)
    const meta = await getMeta()
    const fresh = new VaultSession()
    await expect(fresh.unlock("old passphrase 1234", meta, ITER)).rejects.toMatchObject({ code: "auth-failed" })
    await fresh.unlock("new passphrase 5678", meta, ITER)
    expect((await loadItem(fresh, "chat", chatId)).body).toEqual({ messages: ["hi"] })
    expect((await loadItem(fresh, "persona", personaId)).head).toEqual({ name: "persona" })
  })

  it("aborts without writing when an item cannot be decrypted", async () => {
    const session = new VaultSession()
    await initVault(session, "old passphrase 1234", ITER)
    const { chatId } = await seed(session)
    const rec = store.items.get(`chat/${chatId}`)!
    store.items.set(`chat/${chatId}`, { ...rec, blob: rec.blob.slice(0, -4) + "AAAA" })
    store.puts = 0

    await expect(changePassphrase(session, "old passphrase 1234", "new passphrase 5678", ITER)).rejects.toBeInstanceOf(
      VaultError,
    )
    expect(store.puts).toBe(0)
    expect(store.meta.salt).not.toBeNull()
    const meta = await getMeta()
    const again = new VaultSession()
    await again.unlock("old passphrase 1234", meta, ITER)
  })

  it("rolls back already-written items when a later write fails", async () => {
    const session = new VaultSession()
    await initVault(session, "old passphrase 1234", ITER)
    const { chatId, personaId } = await seed(session)
    const before = new Map([...store.items.entries()].map(([k, v]) => [k, { ...v }]))

    store.puts = 0
    store.failAtPut = 2 // first item write succeeds, second fails
    await expect(changePassphrase(session, "old passphrase 1234", "new passphrase 5678", ITER)).rejects.toThrow(
      "simulated",
    )
    store.failAtPut = null

    // Every item is back to its original ciphertext and still opens with the old passphrase.
    for (const [k, v] of before) {
      const now = store.items.get(k)!
      expect({ head: now.head, blob: now.blob }).toEqual({ head: v.head, blob: v.blob })
    }
    const meta = await getMeta()
    const check = new VaultSession()
    await check.unlock("old passphrase 1234", meta, ITER)
    expect((await loadItem(check, "chat", chatId)).body).toEqual({ messages: ["hi"] })
    expect((await loadItem(check, "persona", personaId)).head).toEqual({ name: "persona" })
  })

  it("rejects a wrong old passphrase before touching anything", async () => {
    const session = new VaultSession()
    await initVault(session, "old passphrase 1234", ITER)
    await seed(session)
    store.puts = 0
    await expect(changePassphrase(session, "not the one", "new passphrase 5678", ITER)).rejects.toMatchObject({
      code: "auth-failed",
    })
    expect(store.puts).toBe(0)
  })
})
