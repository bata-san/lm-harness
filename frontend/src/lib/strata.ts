import type { StrataStatus } from "@/lib/api"

/** Chip states shown on the Strata page. "未インストール" is handled by the page itself (no status to derive). */
export type StrataState = "stopped" | "starting" | "loaded" | "unloaded" | "error"

export const STRATA_STATE_LABEL: Record<StrataState, string> = {
  stopped: "未起動",
  starting: "起動中",
  loaded: "ロード済み",
  unloaded: "アイドル(アンロード済み)",
  error: "エラー",
}

/**
 * Derives the chip state from the backend status.
 * - not running: error when last_error is set, otherwise stopped
 * - running but not ready (and not loaded): starting
 * - running, ready, loaded === false: unloaded (VRAM freed, next request auto-loads)
 * - otherwise: loaded
 */
export function deriveStrataState(s: Pick<StrataStatus, "running" | "instance" | "loaded" | "last_error"> | null | undefined): StrataState {
  if (!s || !s.running) return s?.last_error ? "error" : "stopped"
  const ready = s.instance?.ready === true
  if (s.loaded === true) return "loaded"
  if (!ready) return "starting"
  if (s.loaded === false) return "unloaded"
  return "loaded"
}

/**
 * Seconds left until idle unload, or null when there is nothing to count down
 * (not running, model not loaded, or the limit / idle time is missing or 0).
 * `elapsedS` is the time since the status was received, so the countdown ticks between 2s polls.
 */
export function idleCountdown(s: Pick<StrataStatus, "running" | "loaded" | "idle_for_s" | "idle_limit_s">, elapsedS = 0): number | null {
  if (!s.running || s.loaded === false) return null
  if (s.idle_for_s == null || !s.idle_limit_s || s.idle_limit_s <= 0) return null
  return Math.max(0, Math.ceil(s.idle_limit_s - (s.idle_for_s + Math.max(0, elapsedS))))
}

/** "あと 42 秒でアンロード" (or "まもなくアンロード" at 0). Null when there is no countdown. */
export function countdownLabel(sec: number | null): string | null {
  if (sec == null) return null
  return sec <= 0 ? "まもなくアンロード" : `あと ${sec} 秒でアンロード`
}

/** Warning messages for the top of the page: last error and watchdog restarts that failed. */
export function strataWarnings(s: Pick<StrataStatus, "last_error" | "watchdog"> | null | undefined): string[] {
  if (!s) return []
  const out: string[] = []
  if (s.last_error) out.push(s.last_error)
  if (s.watchdog?.failures > 0) out.push(`ウォッチドッグの再起動に ${s.watchdog.failures} 回失敗しています`)
  return out
}

/** Which primary buttons are enabled for the current status. */
export function strataActions(s: Pick<StrataStatus, "running" | "instance" | "loaded"> | null | undefined, config: string) {
  const running = s?.running === true
  return {
    start: !running && !!config,
    load: running && s?.loaded !== true,
    unload: running && s?.loaded === true,
    stop: running && !!s?.instance,
  }
}
