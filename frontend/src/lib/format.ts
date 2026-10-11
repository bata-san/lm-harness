export function bytes(n: number | null | undefined, digits = 1): string {
  if (n == null || !Number.isFinite(n)) return "—"
  const units = ["B", "KB", "MB", "GB", "TB"]
  let i = 0
  let v = n
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v.toFixed(i === 0 ? 0 : digits)} ${units[i]}`
}

export function duration(sec: number | null | undefined): string {
  if (sec == null || !Number.isFinite(sec)) return "—"
  const s = Math.round(sec)
  if (s < 60) return `${s}s`
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`
}

export function since(ts: number): string {
  return duration(Date.now() / 1000 - ts)
}

export function pct(a: number, b: number): number {
  return b > 0 ? Math.min(100, (a / b) * 100) : 0
}

/** Shell-like split that respects double quotes. */
export function splitArgs(s: string): string[] {
  return (s.match(/"[^"]*"|\S+/g) ?? []).map((t) => t.replace(/^"|"$/g, ""))
}

export function joinArgs(a: string[]): string {
  return a.map((t) => (/\s/.test(t) ? `"${t}"` : t)).join(" ")
}

/** Unix timestamp (seconds or ms) / ISO string -> epoch seconds. */
export function toEpoch(t: number | string | null | undefined): number | null {
  if (t == null) return null
  if (typeof t === "string") {
    const ms = Date.parse(t)
    return Number.isFinite(ms) ? ms / 1000 : null
  }
  return t > 1e12 ? t / 1000 : t
}

export function dateTime(t: number | string | null | undefined): string {
  const e = toEpoch(t)
  return e == null ? "—" : new Date(e * 1000).toLocaleString("ja-JP", { dateStyle: "short", timeStyle: "short" })
}

/** "3 分前" style relative time. */
export function ago(t: number | string | null | undefined): string {
  const e = toEpoch(t)
  if (e == null) return "—"
  const s = Math.max(0, Date.now() / 1000 - e)
  if (s < 60) return "たった今"
  if (s < 3600) return `${Math.floor(s / 60)} 分前`
  if (s < 86400) return `${Math.floor(s / 3600)} 時間前`
  if (s < 86400 * 60) return `${Math.floor(s / 86400)} 日前`
  return dateTime(t)
}

export function compact(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`
  return String(n)
}
