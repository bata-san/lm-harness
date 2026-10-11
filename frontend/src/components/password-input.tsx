import { useState } from "react"
import { EyeIcon, EyeOffIcon } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { cn } from "@/lib/utils"

/** Password field with a show/hide toggle (tap-friendly on phones). */
export function PasswordInput({ className, ...props }: Omit<React.ComponentProps<"input">, "type">) {
  const [show, setShow] = useState(false)
  return (
    <div className="relative">
      <Input {...props} type={show ? "text" : "password"} className={cn("pr-9", className)} autoCapitalize="none" autoCorrect="off" spellCheck={false} />
      <Button
        type="button"
        variant="ghost"
        size="icon-sm"
        className="absolute top-0.5 right-0.5"
        title={show ? "隠す" : "表示"}
        aria-label={show ? "パスワードを隠す" : "パスワードを表示"}
        onClick={() => setShow((s) => !s)}
        tabIndex={-1}
      >
        {show ? <EyeOffIcon /> : <EyeIcon />}
      </Button>
    </div>
  )
}

/** Rough strength estimate for the first-run / change-password hint. */
// eslint-disable-next-line react-refresh/only-export-components
export function passwordStrength(pw: string): { score: 0 | 1 | 2 | 3; label: string } {
  if (!pw) return { score: 0, label: "" }
  let kinds = 0
  if (/[a-z]/.test(pw)) kinds++
  if (/[A-Z]/.test(pw)) kinds++
  if (/\d/.test(pw)) kinds++
  if (/[^A-Za-z0-9]/.test(pw)) kinds++
  if (pw.length < 12) return { score: 0, label: "短すぎます (12 文字以上)" }
  if (new Set(pw).size < 5) return { score: 1, label: "同じ文字が多く推測されやすい" }
  const s = pw.length >= 20 || (pw.length >= 16 && kinds >= 3) ? 3 : kinds >= 3 || pw.length >= 16 ? 2 : 1
  return { score: s as 1 | 2 | 3, label: s === 3 ? "強い" : s === 2 ? "良い" : "弱め (長さか文字種を増やすと安全)" }
}

export function StrengthBar({ password }: { password: string }) {
  const { score, label } = passwordStrength(password)
  if (!password) return null
  const color = ["bg-red-500", "bg-amber-500", "bg-sky-500", "bg-emerald-500"][score]
  return (
    <div className="grid gap-1">
      <div className="flex gap-1">
        {[1, 2, 3].map((i) => (
          <div key={i} className={cn("h-1 flex-1 rounded-full", score >= i ? color : "bg-muted")} />
        ))}
      </div>
      <p className="text-xs text-muted-foreground">{label}</p>
    </div>
  )
}
