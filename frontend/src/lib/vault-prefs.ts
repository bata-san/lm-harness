// Per-viewer convenience for the chat vault. Not secret and not shared: it only remembers the auto-lock period.

/** Auto-lock choices in minutes; 0 means never. */
export const IDLE_CHOICES = [5, 15, 60, 0] as const
const IDLE_KEY = "chat.autoLockMinutes"
const IDLE_DEFAULT = 15

export function readIdlePref(): number {
  try {
    const v = Number(localStorage.getItem(IDLE_KEY))
    if ((IDLE_CHOICES as readonly number[]).includes(v)) return v
  } catch {
    /* storage unavailable */
  }
  return IDLE_DEFAULT
}

export function writeIdlePref(minutes: number): void {
  try {
    localStorage.setItem(IDLE_KEY, String(minutes))
  } catch {
    /* storage unavailable */
  }
}
