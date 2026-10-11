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
