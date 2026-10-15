import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import type { ChatParams } from "@/lib/chat-model"

/** Sampling parameters; an empty box means "use the server default" (the key is omitted). */
export function ParamsFields({ value, onChange, disabled }: { value: ChatParams; onChange: (p: ChatParams) => void; disabled?: boolean }) {
  const num = (raw: string): number | undefined => {
    if (raw.trim() === "") return undefined
    const n = Number(raw)
    return Number.isFinite(n) ? n : undefined
  }
  const set = (key: keyof ChatParams) => (raw: string) => {
    const next = { ...value }
    const n = num(raw)
    if (n === undefined) delete next[key]
    else next[key] = n
    onChange(next)
  }
  return (
    <div className="grid gap-3 sm:grid-cols-3">
      <div className="grid gap-1.5">
        <Label htmlFor="p-temp">temperature</Label>
        <Input id="p-temp" type="number" inputMode="decimal" min={0} max={2} step={0.1} placeholder="既定" disabled={disabled} value={value.temperature ?? ""} onChange={(e) => set("temperature")(e.target.value)} />
      </div>
      <div className="grid gap-1.5">
        <Label htmlFor="p-top">top_p</Label>
        <Input id="p-top" type="number" inputMode="decimal" min={0} max={1} step={0.05} placeholder="既定" disabled={disabled} value={value.top_p ?? ""} onChange={(e) => set("top_p")(e.target.value)} />
      </div>
      <div className="grid gap-1.5">
        <Label htmlFor="p-max">max_tokens</Label>
        <Input id="p-max" type="number" inputMode="numeric" min={1} step={1} placeholder="既定" disabled={disabled} value={value.max_tokens ?? ""} onChange={(e) => set("max_tokens")(e.target.value)} />
      </div>
    </div>
  )
}
