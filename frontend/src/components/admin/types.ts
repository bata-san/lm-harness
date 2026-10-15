// Response shapes of /api/admin/* (see backend/src/admin.rs).
import type { ModeName, ModeProfile } from "@/lib/api"

export type SchemaEntry = { type: string; description: string; restart_required: boolean }

export type ConfigHistoryEntry = { version: string; created_at: string | null; size: number | null }

export type AdminConfig = {
  settings: Record<string, unknown>
  schema: Record<string, SchemaEntry>
  history: ConfigHistoryEntry[]
}

/** Result of PUT /config, POST /config/rollback and POST /config/import. */
export type ConfigSaveResult = {
  ok: boolean
  changed: string[]
  restart_required: string[]
  version: string | null
  settings: Record<string, unknown>
}

export type EnvResponse = { env: Record<string, string>; deny: string[]; name_pattern: string }

export type DirInfo = {
  key: string
  path: string
  exists: boolean
  used_bytes: number
  truncated: boolean
  disk_total_bytes: number | null
  disk_free_bytes: number | null
}

export type ProcessInfo = { pid: number; ppid: number | null; cmd: string; rss_bytes: number | null }

export type SystemdInfo =
  | { available: false }
  | {
      available: true
      scope: string
      unit: string
      active_state: string | null
      sub_state: string | null
      main_pid: number | null
      restarts: number | null
      unit_file_state: string | null
      restart_policy: string | null
    }

export type SystemInfo = {
  hostname: string | null
  kernel: string | null
  os: string | null
  uptime_secs: number
  harness: {
    version: string
    pid: number
    started_at: string
    uptime_secs: number
    binary: string | null
    repo_root: string
    home: string
    under_systemd: boolean
  }
  dirs: DirInfo[]
  processes: ProcessInfo[]
  systemd: SystemdInfo
  tunnel: {
    state: string | null
    provider: string | null
    public_url: string | null
    connected_since: number | string | null
    last_error: string | null
  }
  mode: { current: ModeName; profile: ModeProfile }
}

export type ActionInfo = { id: string; description: string }
export type ActionResult = { ok: boolean; message: string; job_id?: string }

export type LogSource = { id: string; title: string; status?: string }
export type LogsResponse = { source: string; lines: string[] }

export type AuditEntry = { ts: string; ip: string; user: string; action: string; summary: string }
