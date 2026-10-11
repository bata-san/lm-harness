import { toast } from "sonner"

/** Copy text; falls back to execCommand when the Clipboard API is unavailable (plain http, old WebViews). */
export async function copyText(text: string, message = "コピーしました"): Promise<boolean> {
  let ok = false
  try {
    await navigator.clipboard.writeText(text)
    ok = true
  } catch {
    try {
      const ta = document.createElement("textarea")
      ta.value = text
      ta.setAttribute("readonly", "")
      ta.style.position = "fixed"
      ta.style.opacity = "0"
      document.body.appendChild(ta)
      ta.select()
      ok = document.execCommand("copy")
      ta.remove()
    } catch {
      ok = false
    }
  }
  if (ok) toast.success(message)
  else toast.error("コピーできませんでした。手動で選択してコピーしてください")
  return ok
}
