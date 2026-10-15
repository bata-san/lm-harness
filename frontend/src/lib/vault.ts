// Client-side end-to-end encryption for the chat vault.
//
// The passphrase is separate from the login password and never leaves the browser.
// Keys are derived with PBKDF2-SHA-256 and used as non-extractable AES-GCM-256 keys.
// The server only ever stores salts, verifiers and ciphertext.

import { api } from "./api"

export const PBKDF2_ITERATIONS = 600_000
const SALT_BYTES = 16
const IV_BYTES = 12
const GCM_TAG_BYTES = 16
const PREFIX = "v1."
const VERIFIER_PLAINTEXT = "llm-harness-vault-check"
const VERIFIER_KIND = "meta"
const VERIFIER_ID = "verifier"
const DEFAULT_IDLE_MINUTES = 15
const VAULT_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

const te = new TextEncoder()
const td = new TextDecoder()

export type VaultKind = "chat" | "persona"
export const VAULT_KINDS: readonly VaultKind[] = ["chat", "persona"]

export type VaultErrorCode = "bad-format" | "auth-failed"

export class VaultError extends Error {
  readonly code: VaultErrorCode
  constructor(code: VaultErrorCode, message?: string) {
    super(message ?? code)
    this.name = "VaultError"
    this.code = code
  }
}

// ---- base64url (no padding) ----

export function base64urlEncode(bytes: Uint8Array): string {
  let bin = ""
  const CHUNK = 0x8000
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode(...bytes.subarray(i, i + CHUNK))
  }
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "")
}

export function base64urlDecode(s: string): Uint8Array<ArrayBuffer> {
  if (!/^[A-Za-z0-9_-]*$/.test(s) || s.length % 4 === 1) {
    throw new VaultError("bad-format", "invalid base64url")
  }
  const std = s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4)
  const bin = atob(std)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

// ---- keys ----

/** Fresh random 16-byte salt, base64url encoded. */
export function newSalt(): string {
  return base64urlEncode(crypto.getRandomValues(new Uint8Array(SALT_BYTES)))
}

/**
 * PBKDF2-SHA-256 -> AES-GCM-256. The key is not extractable.
 * `iterations` exists for tests only; production code uses the default.
 */
export async function deriveKey(
  passphrase: string,
  saltB64u: string,
  iterations: number = PBKDF2_ITERATIONS,
): Promise<CryptoKey> {
  const salt = base64urlDecode(saltB64u)
  if (salt.length !== SALT_BYTES) throw new VaultError("bad-format", "salt must be 16 bytes")
  const material = await crypto.subtle.importKey(
    "raw",
    te.encode(passphrase.normalize("NFC")),
    "PBKDF2",
    false,
    ["deriveKey"],
  )
  return crypto.subtle.deriveKey(
    { name: "PBKDF2", hash: "SHA-256", salt, iterations },
    material,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  )
}

// ---- JSON encryption ----

function aad(kind: string, id: string): Uint8Array<ArrayBuffer> {
  return te.encode(`${kind}:${id}`)
}

/** Encrypt a JSON-serialisable value. The kind and id are bound as AAD. */
export async function encryptJson(key: CryptoKey, kind: string, id: string, value: unknown): Promise<string> {
  const json = JSON.stringify(value)
  if (json === undefined) throw new TypeError("value is not JSON-serialisable")
  const iv = crypto.getRandomValues(new Uint8Array(IV_BYTES))
  const ct = new Uint8Array(
    await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: aad(kind, id) }, key, te.encode(json)),
  )
  const out = new Uint8Array(IV_BYTES + ct.length)
  out.set(iv, 0)
  out.set(ct, IV_BYTES)
  return PREFIX + base64urlEncode(out)
}

/**
 * Decrypt a blob produced by encryptJson. Throws VaultError:
 * 'bad-format' for malformed input, 'auth-failed' for a wrong key, tampering,
 * or a blob that was moved to a different kind/id.
 */
export async function decryptJson<T>(key: CryptoKey, kind: string, id: string, s: string): Promise<T> {
  if (typeof s !== "string" || !s.startsWith(PREFIX)) throw new VaultError("bad-format", "missing v1 prefix")
  let raw: Uint8Array<ArrayBuffer>
  try {
    raw = base64urlDecode(s.slice(PREFIX.length))
  } catch {
    throw new VaultError("bad-format", "invalid base64url")
  }
  if (raw.length < IV_BYTES + GCM_TAG_BYTES) throw new VaultError("bad-format", "too short")
  const iv = raw.subarray(0, IV_BYTES)
  const ct = raw.subarray(IV_BYTES)
  let pt: ArrayBuffer
  try {
    pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: aad(kind, id) }, key, ct)
  } catch {
    throw new VaultError("auth-failed", "decryption failed")
  }
  try {
    return JSON.parse(td.decode(pt)) as T
  } catch {
    throw new VaultError("bad-format", "plaintext is not JSON")
  }
}

// ---- verifier (lets us tell a wrong passphrase apart without storing it) ----

export function makeVerifier(key: CryptoKey): Promise<string> {
  return encryptJson(key, VERIFIER_KIND, VERIFIER_ID, VERIFIER_PLAINTEXT)
}

/** True only if `key` opens the verifier. Never throws. */
export async function checkVerifier(key: CryptoKey, verifier: string | null | undefined): Promise<boolean> {
  if (!verifier) return false
  try {
    return (await decryptJson<unknown>(key, VERIFIER_KIND, VERIFIER_ID, verifier)) === VERIFIER_PLAINTEXT
  } catch {
    return false
  }
}

// ---- passphrase strength (heuristic only) ----

export type PassphraseStrength = { score: 0 | 1 | 2 | 3; label: string }
const STRENGTH_LABELS = ["短すぎます", "弱い", "普通", "強い"] as const

export function passphraseStrength(p: string): PassphraseStrength {
  const len = [...p].length
  const classes = [/[a-z]/, /[A-Z]/, /[0-9]/, /[^A-Za-z0-9]/].filter((r) => r.test(p)).length
  let score: 0 | 1 | 2 | 3
  if (len < 8) score = 0
  else if (len < 12) score = 1
  else if (len < 16) score = classes >= 2 ? 2 : 1
  else score = classes >= 3 ? 3 : 2
  return { score, label: STRENGTH_LABELS[score] }
}

// ---- in-memory session ----

export type VaultMeta = { v: number; salt: string | null; verifier: string | null }

export class VaultSession {
  #key: CryptoKey | null = null
  #idleMs: number
  #timer: ReturnType<typeof setTimeout> | null = null
  #onLock: (() => void) | undefined

  constructor(opts: { idleMinutes?: number; onLock?: () => void } = {}) {
    this.#idleMs = (opts.idleMinutes ?? DEFAULT_IDLE_MINUTES) * 60_000
    this.#onLock = opts.onLock
  }

  get isUnlocked(): boolean {
    return this.#key !== null
  }

  get key(): CryptoKey | null {
    return this.#key
  }

  /** Derive the key from the passphrase and check it against the stored verifier. */
  async unlock(passphrase: string, meta: VaultMeta, iterations: number = PBKDF2_ITERATIONS): Promise<void> {
    if (!meta.salt || !meta.verifier) throw new Error("vault is not initialized")
    const key = await deriveKey(passphrase, meta.salt, iterations)
    if (!(await checkVerifier(key, meta.verifier))) throw new VaultError("auth-failed", "wrong passphrase")
    this.rekey(key)
  }

  /** Install an already-derived key and start the idle timer. */
  rekey(key: CryptoKey): void {
    this.#key = key
    this.#arm()
  }

  lock(): void {
    this.#clearTimer()
    if (this.#key === null) return
    this.#key = null
    this.#onLock?.()
  }

  /** Set the idle auto-lock period in minutes. 0 disables auto-lock. */
  setIdleMinutes(n: number): void {
    if (!Number.isFinite(n) || n < 0) throw new RangeError("idle minutes must be >= 0")
    this.#idleMs = n * 60_000
    this.#arm()
  }

  /** Record activity; postpones auto-lock. */
  touch(): void {
    if (this.#key !== null) this.#arm()
  }

  #clearTimer(): void {
    if (this.#timer !== null) clearTimeout(this.#timer)
    this.#timer = null
  }

  #arm(): void {
    this.#clearTimer()
    if (this.#key === null || this.#idleMs <= 0) return
    this.#timer = setTimeout(() => this.lock(), this.#idleMs)
  }
}

function requireKey(session: VaultSession): CryptoKey {
  const key = session.key
  if (key === null) throw new Error("vault is locked")
  session.touch()
  return key
}

// ---- HTTP (wraps the existing api() client) ----

export type VaultItemSummary = { id: string; updated_at: string; size: number; head: string }
export type VaultItemRecord = { id: string; updated_at: string; head: string; blob: string }
export type VaultItemWrite = { head: string; blob: string }

export function getMeta(): Promise<VaultMeta> {
  return api<VaultMeta>("/vault/meta")
}

export function putMeta(meta: { salt: string; verifier: string }, replace = false): Promise<void> {
  return api<void>("/vault/meta", {
    method: "PUT",
    json: replace ? { salt: meta.salt, verifier: meta.verifier, replace: true } : meta,
  })
}

export function listItems(kind: VaultKind): Promise<VaultItemSummary[]> {
  return api<VaultItemSummary[]>(`/vault/${kind}`)
}

export function getItem(kind: VaultKind, id: string): Promise<VaultItemRecord> {
  return api<VaultItemRecord>(`/vault/${kind}/${encodeURIComponent(id)}`)
}

export function putItem(kind: VaultKind, id: string, item: VaultItemWrite): Promise<void> {
  return api<void>(`/vault/${kind}/${encodeURIComponent(id)}`, { method: "PUT", json: item })
}

export function deleteItem(kind: VaultKind, id: string): Promise<void> {
  return api<void>(`/vault/${kind}/${encodeURIComponent(id)}`, { method: "DELETE" })
}

export function wipeVault(): Promise<void> {
  return api<void>("/vault", { method: "DELETE" })
}

// ---- higher level ----

function headAad(kind: VaultKind): string {
  return `${kind}.head`
}

function bodyAad(kind: VaultKind): string {
  return `${kind}.body`
}

function assertVaultId(id: string): void {
  if (!VAULT_ID_RE.test(id)) throw new TypeError("vault id must be a lowercase uuid v4")
}

/**
 * First-time setup: new salt, derive key, store salt + verifier (no replace),
 * and unlock the session with the new key.
 */
export async function initVault(
  session: VaultSession,
  passphrase: string,
  iterations: number = PBKDF2_ITERATIONS,
): Promise<void> {
  const salt = newSalt()
  const key = await deriveKey(passphrase, salt, iterations)
  const verifier = await makeVerifier(key)
  await putMeta({ salt, verifier })
  session.rekey(key)
}

/** Encrypt head and body separately (with distinct AAD) and store them. */
export async function saveEncrypted<H, B>(
  session: VaultSession,
  kind: VaultKind,
  id: string,
  head: H,
  body: B,
): Promise<void> {
  assertVaultId(id)
  const key = requireKey(session)
  const [h, b] = await Promise.all([
    encryptJson(key, headAad(kind), id, head),
    encryptJson(key, bodyAad(kind), id, body),
  ])
  await putItem(kind, id, { head: h, blob: b })
}

export type VaultListEntry<H> =
  | { id: string; locked: false; updated_at: string; size: number; head: H }
  | { id: string; locked: true }

/**
 * Decrypt the heads of all items of a kind. An item whose head cannot be
 * decrypted is returned as { id, locked: true } rather than failing the whole list.
 */
export async function loadAll<H = unknown>(session: VaultSession, kind: VaultKind): Promise<VaultListEntry<H>[]> {
  const key = requireKey(session)
  const items = await listItems(kind)
  return Promise.all(
    items.map(async (it): Promise<VaultListEntry<H>> => {
      try {
        const head = await decryptJson<H>(key, headAad(kind), it.id, it.head)
        return { id: it.id, locked: false, updated_at: it.updated_at, size: it.size, head }
      } catch {
        return { id: it.id, locked: true }
      }
    }),
  )
}

/** Fetch and decrypt one item. Unlike loadAll, failures throw. */
export async function loadItem<H = unknown, B = unknown>(
  session: VaultSession,
  kind: VaultKind,
  id: string,
): Promise<{ id: string; updated_at: string; head: H; body: B }> {
  assertVaultId(id)
  const key = requireKey(session)
  const rec = await getItem(kind, id)
  const [head, body] = await Promise.all([
    decryptJson<H>(key, headAad(kind), rec.id, rec.head),
    decryptJson<B>(key, bodyAad(kind), rec.id, rec.blob),
  ])
  return { id: rec.id, updated_at: rec.updated_at, head, body }
}

/**
 * Re-encrypt the whole vault under a new passphrase.
 *
 * Order of operations:
 *   1. Verify oldPass against the stored verifier.
 *   2. Fetch and decrypt every item of every kind, and build all new ciphertexts
 *      in memory. Any failure here aborts before anything is written.
 *   3. PUT each re-encrypted item, then PUT the new salt/verifier with replace.
 *   4. If step 3 fails, the items already written are restored to their original
 *      ciphertext on a best-effort basis, and the error is rethrown.
 *
 * Limitation: the server has no transaction across items and meta. A crash or
 * network loss during step 3 (or a failed rollback) can leave the vault with some
 * items under the new key and the meta still under the old one. The data is not
 * lost, but the affected items will not open until the state is repaired by hand.
 */
export async function changePassphrase(
  session: VaultSession,
  oldPass: string,
  newPass: string,
  iterations: number = PBKDF2_ITERATIONS,
): Promise<void> {
  if (newPass.length === 0) throw new Error("new passphrase is empty")
  const meta = await getMeta()
  if (!meta.salt || !meta.verifier) throw new Error("vault is not initialized")
  const oldKey = await deriveKey(oldPass, meta.salt, iterations)
  if (!(await checkVerifier(oldKey, meta.verifier))) throw new VaultError("auth-failed", "wrong passphrase")

  const newSaltB64 = newSalt()
  const newKey = await deriveKey(newPass, newSaltB64, iterations)
  const newVerifier = await makeVerifier(newKey)

  // Phase 1: decrypt everything and prepare new ciphertexts. No writes yet.
  type Plan = { kind: VaultKind; id: string; original: VaultItemWrite; next: VaultItemWrite }
  const plans: Plan[] = []
  for (const kind of VAULT_KINDS) {
    const items = await listItems(kind)
    for (const it of items) {
      const rec = await getItem(kind, it.id)
      const head = await decryptJson<unknown>(oldKey, headAad(kind), it.id, rec.head)
      const body = await decryptJson<unknown>(oldKey, bodyAad(kind), it.id, rec.blob)
      const [h, b] = await Promise.all([
        encryptJson(newKey, headAad(kind), it.id, head),
        encryptJson(newKey, bodyAad(kind), it.id, body),
      ])
      plans.push({ kind, id: it.id, original: { head: rec.head, blob: rec.blob }, next: { head: h, blob: b } })
    }
  }

  // Phase 2: write items, then meta. Roll back written items if anything fails.
  const written: Plan[] = []
  try {
    for (const p of plans) {
      await putItem(p.kind, p.id, p.next)
      written.push(p)
    }
    await putMeta({ salt: newSaltB64, verifier: newVerifier }, true)
  } catch (e) {
    for (const p of written.reverse()) {
      try {
        await putItem(p.kind, p.id, p.original)
      } catch {
        /* best effort; see the limitation above */
      }
    }
    throw e
  }
  session.rekey(newKey)
}
