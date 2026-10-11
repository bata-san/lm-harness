import { useCallback, useEffect, useRef, useState } from "react"

export async function api<T = unknown>(path: string, init?: RequestInit & { json?: unknown }): Promise<T> {
  const { json, ...rest } = init ?? {}
  const res = await fetch(`/api${path}`, {
    ...rest,
    headers: json !== undefined ? { "content-type": "application/json", ...rest.headers } : rest.headers,
    body: json !== undefined ? JSON.stringify(json) : rest.body,
  })
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`
    try {
      const body = await res.json()
      msg = typeof body.detail === "string" ? body.detail : JSON.stringify(body.detail ?? body)
    } catch {
      /* not json */
    }
    throw new Error(msg)
  }
  return res.json() as Promise<T>
}

/** Poll an endpoint; refetch() forces an immediate reload. */
export function usePoll<T>(path: string | null, intervalMs = 2000) {
  const [data, setData] = useState<T | undefined>()
  const [error, setError] = useState<string | undefined>()
  const alive = useRef(true)

  const refetch = useCallback(async () => {
    if (!path) return
    try {
      const d = await api<T>(path)
      if (alive.current) {
        setData(d)
        setError(undefined)
      }
    } catch (e) {
      if (alive.current) setError((e as Error).message)
    }
  }, [path])

  useEffect(() => {
    alive.current = true
    refetch()
    if (!intervalMs) return
    const id = setInterval(() => {
      if (document.visibilityState === "visible") refetch()
    }, intervalMs)
    return () => {
      alive.current = false
      clearInterval(id)
    }
  }, [refetch, intervalMs])

  return { data, error, refetch }
}

export type Gpu = {
  index: number
  name: string
  memory_total_mb: number
  memory_used_mb: number
  memory_free_mb: number
  util_gpu: number
  util_mem: number
  temperature: number
  power_draw: number | null
  power_limit: number | null
  clock_sm: number
  clock_sm_max: number
  pcie_gen: string
  pcie_width: string
  driver: string
}

export type SystemSnapshot = {
  gpus: Gpu[]
  cpu: {
    model: string
    physical_cores: number
    logical_cores: number
    percent: number
    per_core: number[]
    freq_mhz: number | null
    load_avg: number[]
  }
  memory: { total: number; used: number; available: number; percent: number }
  swap: { total: number; used: number }
  disk: { path: string; total: number; used: number; free: number }
}

export type Check = { id: string; ok: boolean; level: "info" | "warn" | "error"; message: string }

export type GgufInfo = {
  architecture?: string
  name?: string
  size_label?: string
  file_type?: string
  context_length?: number
  block_count?: number
  expert_count?: number | null
  expert_used_count?: number | null
  has_chat_template?: boolean
  error?: string
}

export type LocalModel = {
  id: string
  format: "gguf" | "safetensors"
  path: string
  name: string
  size: number
  files: number
  mmproj?: string | null
  runtimes: string[]
  info: GgufInfo
}

export type DownloadFile = { path: string; size: number; downloaded: number; complete: boolean; connections: number }
export type DownloadStatus = "queued" | "downloading" | "verifying" | "paused" | "completed" | "failed" | "cancelled"
export type Download = {
  id: string
  kind: "hf" | "ollama"
  repo: string
  status: DownloadStatus
  error: string | null
  total: number
  downloaded: number
  speed: number
  eta: number | null
  created_at: number
  finished_at: number | null
  ollama_status: string
  files: DownloadFile[]
}

export type HfRepo = { id: string; downloads: number; likes: number; tags: string[]; pipeline_tag?: string }
export type HfFile = { path: string; size: number; quant: string | null; split: boolean; sha256?: string | null }

export type Metrics = { prompt_tps?: number; gen_tps?: number; processing?: number; deferred?: number; tokens_total?: number; kv_usage?: number }
export type Instance = {
  id: string
  name: string
  cmd: string[]
  status: "starting" | "running" | "exited" | "failed" | "stopped"
  pid: number | null
  returncode: number | null
  started_at: number
  runtime: string
  model: string
  port: number
  host: string
  args: string[]
  ready: boolean
  metrics: Metrics
}

export type Runtime = {
  id: string
  installed: boolean
  path: string | null
  version: string | null
  running?: boolean
  install_job: string
  description: string
}

export type Job = { id: string; title: string; script: string; sudo: boolean; command: string }
export type JobRun = { id: string; name: string; status: Instance["status"]; job: string; started_at: number; returncode: number | null }

export type Profile = {
  id?: string
  name: string
  runtime: string
  model: string
  host?: string | null
  port?: number | null
  args: string[]
  env: Record<string, string>
  autostart: boolean
}

export type TuneResult = {
  args: string[]
  notes: string[]
  estimate: { model_bytes?: number; kv_bytes?: number; vram_free?: number; vram_total?: number; fits?: boolean; ctx?: number; native_ctx?: number }
}

export type Settings = {
  models_dir: string
  hf_token: string
  hf_endpoint: string
  llama_cpp_dir: string
  vllm_venv: string
  ollama_url: string
  max_concurrent_downloads: number
  connections_per_file: number
  default_host: string
  port_range_start: number
  extra_env: Record<string, string>
}

export type LogTail = { lines: { seq: number; text: string }[]; last: number }
