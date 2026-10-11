import { useEffect, useState } from "react"
import { SaveIcon } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, usePoll, type Settings } from "@/lib/api"

const FIELDS: { key: keyof Settings; label: string; hint?: string; type?: "number" | "password" }[] = [
  { key: "models_dir", label: "モデル置き場", hint: "WSL の ext4 上 (~/models 等) を推奨。/mnt/c は 9P 経由で遅い" },
  { key: "hf_token", label: "Hugging Face Token", hint: "gated モデル (Llama, Gemma 等) の取得に必要", type: "password" },
  { key: "hf_endpoint", label: "HF エンドポイント", hint: "ミラーを使う場合に変更" },
  { key: "max_concurrent_downloads", label: "同時ダウンロード数", type: "number" },
  { key: "connections_per_file", label: "1 ファイルあたりの接続数", hint: "回線が太いなら 8〜16", type: "number" },
  { key: "llama_cpp_dir", label: "llama.cpp ディレクトリ" },
  { key: "vllm_venv", label: "vLLM venv" },
  { key: "ollama_url", label: "Ollama URL" },
  { key: "default_host", label: "サーバの bind アドレス", hint: "0.0.0.0 で LAN からも接続可。ローカル限定なら 127.0.0.1" },
  { key: "port_range_start", label: "ポート自動割当の開始番号", type: "number" },
]

export function SettingsPage() {
  const { data } = usePoll<Settings>("/settings", 0)
  const [form, setForm] = useState<Partial<Settings>>({})

  useEffect(() => {
    if (data) setForm(data)
  }, [data])

  async function save() {
    try {
      const patch = Object.fromEntries(
        FIELDS.map((f) => [f.key, f.type === "number" ? Number(form[f.key]) : form[f.key]]),
      )
      setForm(await api<Settings>("/settings", { method: "PUT", json: patch }))
      toast.success("保存しました")
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  return (
    <Card className="max-w-3xl">
      <CardHeader>
        <CardTitle>設定</CardTitle>
        <CardDescription>~/.llm-harness/settings.json に保存されます</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">
        {FIELDS.map((f) => (
          <div key={f.key} className="grid gap-1.5">
            <Label htmlFor={f.key}>{f.label}</Label>
            <Input
              id={f.key}
              type={f.type === "password" ? "password" : f.type === "number" ? "number" : "text"}
              value={String(form[f.key] ?? "")}
              onChange={(e) => setForm((s) => ({ ...s, [f.key]: e.target.value }))}
            />
            {f.hint && <p className="text-xs text-muted-foreground">{f.hint}</p>}
          </div>
        ))}
      </CardContent>
      <CardFooter>
        <Button onClick={save}>
          <SaveIcon /> 保存
        </Button>
      </CardFooter>
    </Card>
  )
}
