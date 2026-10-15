import { describe, expect, it } from "vitest"

import type { StrataStatus } from "./api"
import { countdownLabel, deriveStrataState, idleCountdown, strataActions, strataWarnings } from "./strata"

const base: StrataStatus = {
  running: false,
  instance: null,
  loaded: null,
  auto_load: null,
  idle_for_s: null,
  idle_limit_s: null,
  configs: ["qwen-moe"],
  last_error: null,
  watchdog: { enabled: true, failures: 0 },
}
const inst = { id: "i1", name: "strata", config: "qwen-moe", port: 8080, ready: true }
const running = (over: Partial<StrataStatus> = {}): StrataStatus => ({ ...base, running: true, instance: inst, loaded: true, ...over })

describe("deriveStrataState", () => {
  it("is 未起動 when nothing runs and there is no error", () => {
    expect(deriveStrataState(base)).toBe("stopped")
    expect(deriveStrataState(undefined)).toBe("stopped")
  })

  it("is エラー when stopped with last_error", () => {
    expect(deriveStrataState({ ...base, last_error: "CUDA out of memory" })).toBe("error")
  })

  it("is 起動中 while the process is up but not ready", () => {
    expect(deriveStrataState(running({ loaded: null, instance: { ...inst, ready: false } }))).toBe("starting")
    expect(deriveStrataState(running({ loaded: false, instance: null }))).toBe("starting")
  })

  it("is ロード済み when the model is loaded", () => {
    expect(deriveStrataState(running({ loaded: true }))).toBe("loaded")
  })

  it("is アイドル(アンロード済み) when running, ready and not loaded", () => {
    expect(deriveStrataState(running({ loaded: false }))).toBe("unloaded")
  })

  it("treats an unknown loaded flag on a ready instance as loaded", () => {
    expect(deriveStrataState(running({ loaded: null }))).toBe("loaded")
  })

  it("keeps the running state when last_error is set but the process is up", () => {
    expect(deriveStrataState(running({ last_error: "watchdog: restarted" }))).toBe("loaded")
  })
})

describe("idleCountdown", () => {
  it("counts down from the idle limit", () => {
    expect(idleCountdown(running({ idle_for_s: 100, idle_limit_s: 600 }))).toBe(500)
  })

  it("adds the time elapsed since the status arrived", () => {
    expect(idleCountdown(running({ idle_for_s: 100, idle_limit_s: 600 }), 30)).toBe(470)
  })

  it("never goes below zero", () => {
    expect(idleCountdown(running({ idle_for_s: 590, idle_limit_s: 600 }), 60)).toBe(0)
  })

  it("is hidden when idle_for_s or idle_limit_s is null or 0", () => {
    expect(idleCountdown(running({ idle_for_s: null, idle_limit_s: 600 }))).toBeNull()
    expect(idleCountdown(running({ idle_for_s: 10, idle_limit_s: null }))).toBeNull()
    expect(idleCountdown(running({ idle_for_s: 10, idle_limit_s: 0 }))).toBeNull()
  })

  it("is hidden when not running or already unloaded", () => {
    expect(idleCountdown({ ...base, idle_for_s: 10, idle_limit_s: 600 })).toBeNull()
    expect(idleCountdown(running({ loaded: false, idle_for_s: 10, idle_limit_s: 600 }))).toBeNull()
  })
})

describe("countdownLabel", () => {
  it("formats the remaining seconds", () => {
    expect(countdownLabel(42)).toBe("あと 42 秒でアンロード")
    expect(countdownLabel(0)).toBe("まもなくアンロード")
    expect(countdownLabel(null)).toBeNull()
  })
})

describe("strataWarnings", () => {
  it("lists last_error and watchdog failures", () => {
    expect(strataWarnings({ last_error: "boom", watchdog: { enabled: true, failures: 2 } })).toEqual([
      "boom",
      "ウォッチドッグの再起動に 2 回失敗しています",
    ])
  })

  it("is empty when everything is healthy", () => {
    expect(strataWarnings(base)).toEqual([])
    expect(strataWarnings(undefined)).toEqual([])
  })
})

describe("strataActions", () => {
  it("enables start only when stopped and a config is chosen", () => {
    expect(strataActions(base, "qwen-moe").start).toBe(true)
    expect(strataActions(base, "").start).toBe(false)
    expect(strataActions(running(), "qwen-moe").start).toBe(false)
  })

  it("enables unload only when the model is loaded", () => {
    expect(strataActions(running({ loaded: true }), "").unload).toBe(true)
    expect(strataActions(running({ loaded: false }), "").unload).toBe(false)
    expect(strataActions(running({ loaded: false }), "").load).toBe(true)
    expect(strataActions(running({ loaded: true }), "").load).toBe(false)
  })

  it("enables stop only when an instance exists", () => {
    expect(strataActions(running(), "").stop).toBe(true)
    expect(strataActions(running({ instance: null }), "").stop).toBe(false)
    expect(strataActions(base, "").stop).toBe(false)
  })

  it("disables everything while stopped except start", () => {
    const a = strataActions(base, "qwen-moe")
    expect([a.load, a.unload, a.stop]).toEqual([false, false, false])
  })
})
