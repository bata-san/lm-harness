import { useCallback, useEffect, useRef, useState } from "react"

export class ApiError extends Error {
  status: number
  totpRequired: boolean
  constructor(message: string, status: number, totpRequired = false) {
    super(message)
    this.name = "ApiError"
    this.status = status
    this.totpRequired = totpRequired
  }
}

// Any 401 is broadcast so the AuthGate can re-check the session and show the login screen.
const unauthorizedListeners = new Set<() => void>()
export function onUnauthorized(fn: () => void): () => void {
  unauthorizedListeners.add(fn)
  return () => {
    unauthorizedListeners.delete(fn)
  }
}

export async function api<T = unknown>(path: string, init?: RequestInit & { json?: unknown }): Promise<T> {
  const { json, headers, ...rest } = init ?? {}
  const h = new Headers(headers)
  if (json !== undefined) h.set("content-type", "application/json")
  // CSRF guard required by the backend for every request (harmless on GET).
  h.set("X-Harness-CSRF", "1")
  const res = await fetch(`/api${path}`, {
    ...rest,
    credentials: "same-origin",
    headers: h,
    body: json !== undefined ? JSON.stringify(json) : rest.body,
  })
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`
    let totp = false
    try {
      const body = await res.json()
      msg = typeof body.detail === "string" ? body.detail : JSON.stringify(body.detail ?? body)
      totp = body.totp_required === true
    } catch {
      /* not json */
    }
    if (res.status === 401) unauthorizedListeners.forEach((fn) => fn())
    throw new ApiError(msg, res.status, totp)
  }
  if (res.status === 204) return undefined as T
  const text = await res.text()
  return (text ? JSON.parse(text) : undefined) as T
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
  power_default_limit?: number | null
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
  auto_tune?: boolean
  mode?: ModeName
  stale?: boolean
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
  auto_tune?: boolean
}

export type TuneResult = {
  args: string[]
  notes: string[]
  estimate: { model_bytes?: number; kv_bytes?: number; vram_free?: number; vram_total?: number; vram_reserve?: number; fits?: boolean; ctx?: number; native_ctx?: number }
}

export type Settings = {
  models_dir: string
  hf_token: string
  hf_endpoint: string
  llama_cpp_dir: string
  vllm_venv: string
  strata_dir: string
  ollama_url: string
  max_concurrent_downloads: number
  connections_per_file: number
  default_host: string
  port_range_start: number
  extra_env: Record<string, string>
}

export type LogTail = { lines: { seq: number; text: string }[]; last: number }

export type ModeName = "full" | "shared"

export type ModeProfile = {
  mode: ModeName
  cpu_set: number[]
  physical_cores: number
  logical_cpus: number
  vram_reserve_mb: number
  gpu_power_percent: number
  ubatch_cap: number
  nice: number
  max_concurrent_downloads: number
  connections_per_file: number
  download_bytes_per_sec: number | null
  vmmem_priority: string
}

export type SharedLimits = {
  cpu_fraction: number
  vram_reserve_mb: number
  gpu_power_percent: number
  ubatch_cap: number
  nice: number
  max_concurrent_downloads: number
  connections_per_file: number
  download_mib_per_sec: number | null
  vmmem_below_normal: boolean
}

export type Schedule = { enabled: boolean; full_start: string; full_end: string; weekends_full: boolean }

export type ModeSettings = { current: ModeName; auto_restart: boolean; shared: SharedLimits; schedule: Schedule }

export type ModeSource = "manual" | "schedule" | "startup" | "settings"

export type ModeState = {
  mode: ModeName
  source: ModeSource
  switched_at: number | null
  profile: ModeProfile
  settings: ModeSettings
  scheduled_mode: ModeName | null
  next_switch_at?: number | null
  bridge: { available: boolean; message: string; last_result: unknown | null }
  restarted: string[]
  topology: { physical_cores: number; logical_cores: number }
}

// ---- auth ----
export type AuthStatus = {
  configured: boolean
  authenticated: boolean
  user: string | null
  totp_enabled: boolean
  local: boolean
  principal_kind: string | null
}
export type AuthSession = { id: string; created_at: number; last_seen_at: number; ip: string; user_agent: string; current: boolean }
/** "openai" -> sk-lh-…, "anthropic" -> sk-ant-lh-…, "generic" -> lh_… (legacy, still valid). */
export type ApiKeyKind = "openai" | "anthropic" | "generic"
/** GET /api/auth/keys row (never contains the secret). */
export type ApiKey = {
  id: string
  name: string
  kind: ApiKeyKind
  prefix: string
  created_at: number
  last_used: number | null
  last_used_at: number | null
}
/** POST /api/auth/keys response; `key` is the full secret and is returned only here. */
export type CreatedApiKey = { id: string; name: string; kind: ApiKeyKind; key: string; prefix: string; created_at: number }

// ---- tunnel ----
export type TunnelProvider = "cloudflare_quick" | "cloudflare_token" | "ngrok"
export type TunnelState = "stopped" | "installing" | "starting" | "connected" | "degraded" | "restarting" | "error"
export type TunnelConfig = {
  provider: TunnelProvider
  autostart: boolean
  cloudflare_token: string
  cloudflare_hostname: string
  ngrok_authtoken: string
  ngrok_domain: string
}
export type TunnelStatus = {
  state: TunnelState
  provider: TunnelProvider
  public_url: string | null
  connected_since: number | null
  restarts: number
  last_error: string | null
  last_health: { ok: boolean; at: number; latency_ms: number } | null
  auth_configured: boolean
  binaries: { cloudflared: { path: string; version: string | null } | null; ngrok: { path: string; version: string | null } | null }
  config: TunnelConfig
}

// ---- catalog ----
export type Fit = "gpu" | "moe_offload" | "partial" | "cpu" | "too_big"
export type CatalogItem = {
  id: string
  downloads: number
  likes: number
  updated: string | number | null
  created_at: string | number | null
  pipeline_tag?: string | null
  tags: string[]
  installed: boolean
  best: { quant: string; size: number; fit: Fit; files: number } | null
}
export type CatalogPlan = {
  repo: string
  recommended_quant: string | null
  options: { quant: string; size: number; files: { path: string; size: number; sha256?: string | null }[]; fit: Fit; recommended: boolean; note: string }[]
}
export type UninstallResult = { stopped: string[]; profiles_removed: number; freed_bytes: number }
