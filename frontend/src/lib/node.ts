/**
 * The fleet node the UI is currently showing. It comes from the URL
 * (`/n/<node>/…`; no prefix = the home server) and is set by Layout while
 * rendering, before any page renders or fetches.
 */
export const HOME = 'home'

let current = HOME
let currentEnv: string | undefined

export function setCurrentNode(node: string, env?: string) {
  current = node
  currentEnv = env
}

export function currentNode() {
  return current
}

/** Master-level endpoints: never forwarded to a node. */
const GLOBAL_PREFIXES = ['/auth', '/setup', '/admin', '/fleet', '/health']

/** API path as seen by the browser: node-local calls go through the tunnel. */
export function apiPath(path: string, node = current): string {
  if (node === HOME || GLOBAL_PREFIXES.some((p) => path.startsWith(p))) return path
  return `/nodes/${encodeURIComponent(node)}${path}`
}

/** App (router) path on a node: `/projects` → `/n/do-1/projects`. */
export function nodePath(path: string, node = current): string {
  return node === HOME ? path : `/n/${encodeURIComponent(node)}${path}`
}

/** The node a router pathname belongs to. */
export function nodeFromPathname(pathname: string): string {
  const m = /^\/n\/([^/]+)/.exec(pathname)
  return m ? decodeURIComponent(m[1]) : HOME
}

/** The pathname without its `/n/<node>` prefix. */
export function stripNode(pathname: string): string {
  return pathname.replace(/^\/n\/[^/]+/, '') || '/'
}

export interface FleetNode {
  name: string
  env: string
  color: string | null
  local: boolean
  online: boolean
  hostname: string | null
  version: string | null
  allow?: string[] | null
  interval?: number
  last_seen?: number | null
  connected_at?: number | null
  /** `[ts, cpu, mem_used, rx, tx, disk_r, disk_w, load1]` */
  last: number[] | null
  summary: {
    mem_total: number
    disk_total: number
    disk_used: number
    cores: number
    uptime: number
    cpu_temp: number | null
  } | null
  spark: number[]
  events?: { ts: number; kind: string; detail: Record<string, unknown> }[]
  can_system?: boolean
}

/** Does this node's agent allow a capability? Home allows everything. */
export function nodeAllows(node: FleetNode | undefined, cap: string): boolean {
  if (!node || node.local) return true
  return !!node.allow?.includes(cap)
}

/** " on do-1 (PROD)" for confirmations; empty on the home server. */
export function onNodeLabel(node = current, env = node === current ? currentEnv : undefined): string {
  if (node === HOME) return ''
  return env ? ` on ${node} (${env.toUpperCase()})` : ` on ${node}`
}

/**
 * Confirm an action that changes a remote node. On the home server it asks
 * only when `always` (e.g. compose down); on a droplet every state change
 * names the node and its environment, so a click never lands on prod by
 * surprise.
 */
export function confirmOnNode(what: string, always = false): boolean {
  if (current === HOME && !always) return true
  return confirm(`${what}${onNodeLabel()}?`)
}
