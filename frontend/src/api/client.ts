export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

export async function api<T = unknown>(
  path: string,
  opts: { method?: string; body?: unknown } = {},
): Promise<T> {
  const method = opts.method ?? 'GET'
  const headers: Record<string, string> = {}
  if (method !== 'GET') {
    headers['X-CSRF'] = '1'
    if (opts.body !== undefined) headers['Content-Type'] = 'application/json'
  }
  const res = await fetch(`/api${path}`, {
    method,
    credentials: 'same-origin',
    headers,
    body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
  })
  if (res.status === 401 && !path.startsWith('/auth') && !path.startsWith('/setup')) {
    window.location.href = '/login'
    throw new ApiError(401, 'unauthorized')
  }
  if (!res.ok) {
    let message = res.statusText
    try {
      const data = await res.json()
      if (data?.error) message = data.error
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(res.status, message)
  }
  if (res.status === 204) return undefined as T
  return res.json() as Promise<T>
}

export function wsUrl(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${proto}://${location.host}/api${path}`
}

// ---- shared types ----

export interface ProjectPerms {
  view: boolean
  control: boolean
  logs: boolean
  files: boolean
  git: boolean
}

export interface GitFile {
  path: string
  status: string
  conflicted: boolean
}

export interface GitCommit {
  hash: string
  author: string
  when: string
  subject: string
}

export interface GitStatus {
  is_repo: boolean
  branch?: string
  ahead?: number | null
  behind?: number | null
  in_merge?: boolean
  files?: GitFile[]
  log?: GitCommit[]
  stash_count?: number
}

export interface GitOutput {
  ok: boolean
  output: string
}

export interface Permissions {
  system: boolean
  projects: Record<string, ProjectPerms>
}

export interface SessionInfo {
  authenticated: boolean
  username: string | null
  role: 'admin' | 'user' | null
  permissions: Permissions
}

export const can = (
  s: SessionInfo | undefined,
  cap: keyof ProjectPerms,
  project: string,
): boolean => s?.role === 'admin' || !!s?.permissions.projects[project]?.[cap]

export const anyProject = (
  s: SessionInfo | undefined,
  cap: keyof ProjectPerms,
): boolean =>
  s?.role === 'admin' ||
  Object.values(s?.permissions.projects ?? {}).some((p) => p[cap])

export interface AdminUser {
  id: number
  username: string
  role: 'admin' | 'user'
  totp_confirmed: boolean
  must_change_password: boolean
  created_at: string
  permissions: Permissions
}

export interface Project {
  name: string
  path: string
  compose_files: string[]
  status: 'running' | 'partial' | 'stopped' | 'not-created'
  running: number
  total: number
}

export interface ContainerInfo {
  id: string
  name: string
  service: string
  image: string
  state: string
  status: string
}

export interface Job {
  id: number
  action: string
  project: string
  status: 'running' | 'done' | 'failed'
  output: string
}

export interface HostInfo {
  hostname: string | null
  os: string | null
  kernel: string | null
  cpu_brand: string | null
  cores: number
  physical_cores: number | null
}

export interface ProcessInfo {
  pid: number
  name: string
  cpu: number
  mem: number
}

export interface SystemStats {
  ts: number
  host?: HostInfo
  cpu: { total: number; per_core: number[]; temp?: number | null }
  mem: {
    total: number
    used: number
    available?: number
    swap_total: number
    swap_used: number
  }
  disks: { mount: string; total: number; used: number }[]
  disk_io?: { read_rate: number; write_rate: number }
  net: { iface: string; rx_rate: number; tx_rate: number; rx_total?: number; tx_total?: number }[]
  temps?: { label: string; temp: number; critical: number | null }[]
  processes?: { count: number; top: ProcessInfo[] }
  load: [number, number, number]
  uptime: number
}

/** `[unix_secs, cpu_pct, mem_used, rx_rate, tx_rate, disk_read_rate, disk_write_rate, load1]` */
export type HistoryPoint = [number, number, number, number, number, number, number, number]

export interface ServiceStatus {
  unit: string
  active_state: string
  sub_state: string
  since: string
}

export interface FileEntry {
  name: string
  type: 'dir' | 'file' | 'symlink'
  size: number
  mtime_ms: number
}

export interface AuditEntry {
  id: number
  ts: string
  ip: string
  actor: string
  action: string
  detail: string
  ok: boolean
}
