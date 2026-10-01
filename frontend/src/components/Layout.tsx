import { useEffect, useMemo, useRef, useState } from 'react'
import { QueryClient, QueryClientProvider, useQuery } from '@tanstack/react-query'
import { Navigate, NavLink, Outlet, useLocation, useNavigate } from 'react-router'
import { anyProject, api } from '../api/client'
import type { ServiceStatus, SessionInfo } from '../api/client'
import { StatsProvider, useStats } from '../lib/stats'
import { fmtBytes, fmtRate, fmtUptime, level, levelText } from '../lib/format'
import {
  HOME,
  nodeAllows,
  nodeFromPathname,
  nodePath,
  setCurrentNode,
  stripNode,
} from '../lib/node'
import type { FleetNode } from '../lib/node'
import CommandPalette from './CommandPalette'

export function useSession() {
  return useQuery({
    queryKey: ['session'],
    queryFn: () => api<SessionInfo>('/auth/session'),
  })
}

/** Every node the user can see, home first. Polled for the selector and overview. */
export function useFleetNodes(enabled = true) {
  return useQuery({
    queryKey: ['fleet-nodes'],
    queryFn: () => api<FleetNode[]>('/fleet/nodes'),
    refetchInterval: 10_000,
    enabled,
  })
}

/** The fleet entry for the node being shown (undefined on home before the list loads). */
export function useCurrentNode(): FleetNode | undefined {
  const location = useLocation()
  const fleet = useFleetNodes()
  const name = nodeFromPathname(location.pathname)
  return fleet.data?.find((n) => n.name === name)
}

export interface ChromeContext {
  chromeHidden: boolean
  setChromeHidden: (hidden: boolean) => void
}

export const canSystem = (s: SessionInfo | undefined) =>
  s?.role === 'admin' || !!s?.permissions.system

interface NavItem {
  to: string
  label: string
  /** Master-level page: same on every node, never prefixed. */
  global?: boolean
}

/** Tabs for one node, filtered by the user's permissions and by what that
 *  node's agent allows (SS_AGENT_ALLOW). */
export function navFor(s: SessionInfo, node?: FleetNode): NavItem[] {
  const admin = s.role === 'admin'
  return [
    { to: '/projects', label: 'projects', show: anyProject(s, 'view') && nodeAllows(node, 'projects') },
    { to: '/system', label: 'system', show: canSystem(s) && nodeAllows(node, 'system') },
    { to: '/terminal', label: 'term', show: admin && nodeAllows(node, 'terminal') },
    { to: '/files', label: 'files', show: anyProject(s, 'files') && nodeAllows(node, 'files') },
    { to: '/fleet', label: 'fleet', show: true, global: true },
    { to: '/events', label: 'events', show: true, global: true },
    { to: '/users', label: 'users', show: admin, global: true },
    { to: '/audit', label: 'audit', show: admin && nodeAllows(node, 'system') },
  ]
    .filter((n) => n.show)
    .map(({ to, label, global }) => ({ to, label, global }))
}

const linkFor = (n: NavItem) => (n.global ? n.to : nodePath(n.to))

/// Index route: the fleet overview is the landing page.
export function Home() {
  const session = useSession()
  if (!session.data) return null
  return <Navigate to="/fleet" replace />
}

const SPARK = '▁▂▃▄▅▆▇█'

/** A react-query cache per node: `['projects']` on do-1 must never show home's. */
const nodeClients = new Map<string, QueryClient>()
function clientFor(node: string) {
  let c = nodeClients.get(node)
  if (!c) {
    c = new QueryClient({ defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: false } } })
    nodeClients.set(node, c)
  }
  return c
}

function EnvTag({ node }: { node: FleetNode | undefined }) {
  if (!node || node.local) return null
  return (
    <span
      className="px-1 text-[10px] font-bold uppercase text-zinc-950"
      style={{ background: node.color ?? '#a1a1aa' }}
    >
      {node.env}
    </span>
  )
}

function Dot({ online }: { online: boolean }) {
  return <span className={online ? 'text-emerald-400' : 'text-zinc-600'}>●</span>
}

/** Header environment selector: home, each droplet, and the fleet overview. */
function NodeSelector({ nodes, current }: { nodes: FleetNode[]; current: string }) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement>(null)
  const navigate = useNavigate()
  const location = useLocation()
  const node = nodes.find((n) => n.name === current)

  useEffect(() => {
    if (!open) return
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false)
    }
    window.addEventListener('mousedown', close)
    return () => window.removeEventListener('mousedown', close)
  }, [open])

  function pick(target: string) {
    setOpen(false)
    // Stay on the same section when switching nodes (`/projects/x` → `/projects`).
    const section = stripNode(location.pathname).split('/')[1]
    const keep = ['system', 'projects', 'files', 'terminal', 'audit'].includes(section)
    navigate(nodePath(keep ? `/${section}` : '/system', target))
  }

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex h-7 items-center gap-2 border border-zinc-700 bg-zinc-900 px-2 hover:border-zinc-500"
        title="switch node (⌘K → node …)"
      >
        <Dot online={node?.online ?? true} />
        <span className="text-zinc-100">{current}</span>
        <EnvTag node={node} />
        <span className="text-zinc-500">▾</span>
      </button>
      {open && (
        <div className="absolute left-0 top-8 z-40 w-72 border border-zinc-700 bg-zinc-950 shadow-xl">
          {nodes.map((n) => (
            <button
              key={n.name}
              type="button"
              onClick={() => pick(n.name)}
              className={`flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-zinc-900 ${
                n.name === current ? 'bg-zinc-900' : ''
              }`}
            >
              <Dot online={n.online} />
              <span className="flex-1 text-zinc-100">{n.name}</span>
              {n.local ? <span className="text-[10px] text-zinc-500">master</span> : <EnvTag node={n} />}
              <span className="w-10 text-right tabular-nums text-zinc-400">
                {n.online && n.last ? `${n.last[1].toFixed(0)}%` : n.online ? '' : 'off'}
              </span>
            </button>
          ))}
          <button
            type="button"
            onClick={() => {
              setOpen(false)
              navigate('/fleet')
            }}
            className="w-full border-t border-zinc-800 px-3 py-1.5 text-left text-accent hover:bg-zinc-900"
          >
            ⊞ fleet overview
          </button>
        </div>
      )}
    </div>
  )
}

function StatusLine({
  username,
  showSystem,
  node,
}: {
  username: string | null
  showSystem: boolean
  node: FleetNode | undefined
}) {
  const { stats, history, stale, connected } = useStats()
  const services = useQuery({
    queryKey: ['services'],
    queryFn: () => api<ServiceStatus[]>('/system/services'),
    refetchInterval: 10_000,
    enabled: showSystem,
  })
  const down = services.data?.filter((s) => s.active_state !== 'active') ?? []
  const tunnel = services.data?.find((s) => s.unit.startsWith('cloudflared'))

  const spark = history
    .slice(-8)
    .map((p) => SPARK[Math.min(7, Math.floor((p[1] / 100) * 8))])
    .join('')
  const root = stats?.disks.find((d) => d.mount === '/') ?? stats?.disks[0]
  const remote = node && !node.local
  // The environment's colour paints the whole line: you always know which
  // machine a click is about to hit.
  const style = remote && node.color ? { background: node.color } : undefined

  return (
    <footer
      className="flex h-7 shrink-0 items-center gap-5 overflow-hidden whitespace-nowrap bg-accent px-4 text-xs font-medium text-zinc-950"
      style={style}
    >
      {remote && (
        <span className="font-bold uppercase">
          {node.name} · {node.env}
        </span>
      )}
      {showSystem && stats ? (
        <>
          {(stale || !connected) && <span className="font-bold">▲ STALE</span>}
          <span>
            cpu {stats.cpu.total.toFixed(0)}% {spark}
          </span>
          <span>
            mem {fmtBytes(stats.mem.used)}/{fmtBytes(stats.mem.total)}
          </span>
          {root && (
            <span>
              {root.mount} {((root.used / root.total) * 100).toFixed(0)}%
            </span>
          )}
          <span>
            ↓{fmtRate(stats.net.reduce((a, n) => a + n.rx_rate, 0))} ↑
            {fmtRate(stats.net.reduce((a, n) => a + n.tx_rate, 0))}
          </span>
          <span>load {stats.load[0].toFixed(2)}</span>
          {stats.cpu.temp != null && <span>{stats.cpu.temp.toFixed(0)}°C</span>}
        </>
      ) : (
        !remote && <span>serious-server</span>
      )}
      <div className="flex-1" />
      {down.length > 0 && <span className="font-bold">▲ {down.map((s) => s.unit).join(', ')} down</span>}
      {tunnel && <span>tunnel {tunnel.active_state === 'active' ? 'ok' : tunnel.active_state}</span>}
      {showSystem && stats && <span>up {fmtUptime(stats.uptime)}</span>}
      {username && <span>{username}</span>}
    </footer>
  )
}

function CpuBadge() {
  const { stats } = useStats()
  if (!stats) return null
  return (
    <span className={`tabular-nums ${levelText[level(stats.cpu.total, 75, 90)]}`}>
      {stats.cpu.total.toFixed(0)}%
    </span>
  )
}

function ago(ts: number | null | undefined) {
  if (!ts) return 'never'
  const s = Math.max(0, Math.floor(Date.now() / 1000 - ts))
  if (s < 90) return `${s}s ago`
  if (s < 5400) return `${Math.floor(s / 60)} min ago`
  if (s < 172800) return `${Math.floor(s / 3600)} h ago`
  return `${Math.floor(s / 86400)} days ago`
}

export default function Layout() {
  const navigate = useNavigate()
  const location = useLocation()
  const session = useSession()
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [chromeHidden, setChromeHidden] = useState(false)

  // Set before any child renders or fetches: api()/wsUrl() read it.
  const current = nodeFromPathname(location.pathname)
  const fleet = useFleetNodes(!!session.data?.authenticated)
  const nodes = fleet.data ?? []
  const node = nodes.find((n) => n.name === current)
  setCurrentNode(current, node && !node.local ? node.env : undefined)

  const nav = useMemo(
    () => (session.data ? navFor(session.data, current === HOME ? undefined : node) : []),
    [session.data, node, current],
  )

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        setPaletteOpen((o) => !o)
        return
      }
      const t = e.target as HTMLElement
      if (t.closest('input, textarea, select, [contenteditable]')) return
      if (e.metaKey || e.ctrlKey || e.altKey) return
      const i = Number(e.key)
      if (i >= 1 && i <= nav.length) navigate(linkFor(nav[i - 1]))
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [nav, navigate])

  if (session.isLoading) {
    return (
      <div className="flex h-screen items-center justify-center text-zinc-500">loading…</div>
    )
  }
  if (session.data && !session.data.authenticated) {
    window.location.href = '/login'
    return null
  }

  async function logout() {
    await api('/auth/logout', { method: 'POST' })
    navigate('/login')
  }

  const remote = current !== HOME
  const offline = remote && fleet.isSuccess && !node?.online
  const showSystem =
    canSystem(session.data) && nodeAllows(remote ? node : undefined, 'system') && !offline
  const username = session.data?.username ?? null
  const selectorNodes: FleetNode[] = nodes.length
    ? nodes
    : [{ name: HOME, env: 'home', color: null, local: true, online: true } as FleetNode]

  return (
    <QueryClientProvider client={clientFor(current)}>
      <StatsProvider key={current} enabled={showSystem}>
        <div className="flex h-screen flex-col">
          {!chromeHidden && (
            <header className="flex h-11 shrink-0 items-center gap-4 border-b border-zinc-800 px-4">
              <span className="font-bold text-accent">serious://</span>
              <NodeSelector nodes={selectorNodes} current={current} />
              {showSystem && <CpuBadge />}
              <nav className="ml-2 flex h-full items-stretch gap-0.5">
                {nav.map((n, i) => (
                  <NavLink
                    key={n.to}
                    to={linkFor(n)}
                    className={({ isActive }) =>
                      `flex items-center border-t-2 px-3 transition-colors ${
                        isActive
                          ? 'border-accent bg-zinc-900 text-zinc-50'
                          : 'border-transparent text-zinc-500 hover:text-zinc-200'
                      }`
                    }
                  >
                    {i + 1}:{n.label}
                  </NavLink>
                ))}
              </nav>
              <div className="flex-1" />
              <button
                type="button"
                onClick={() => setPaletteOpen(true)}
                className="flex h-7 w-72 items-center gap-2 border border-zinc-700 bg-zinc-900 px-2.5 text-left text-zinc-500 hover:border-zinc-600"
              >
                <span className="text-accent">&gt;</span>
                <span className="flex-1">run a command…</span>
                <span className="border border-zinc-700 px-1 text-[10px]">⌘K</span>
              </button>
              <button type="button" onClick={logout} className="text-zinc-500 hover:text-zinc-200">
                logout
              </button>
            </header>
          )}
          {offline && (
            <div className="border-b border-red-900 bg-red-950 px-4 py-1.5 text-sm text-red-200">
              ▲ {current} is offline — last seen {ago(node?.last_seen)}. Actions are unavailable until it
              reconnects.
            </div>
          )}
          <main className="min-h-0 flex-1 overflow-auto">
            <Outlet context={{ chromeHidden, setChromeHidden } satisfies ChromeContext} />
          </main>
          {!chromeHidden && <StatusLine username={username} showSystem={showSystem} node={node} />}
        </div>
        {paletteOpen && session.data && (
          <CommandPalette
            session={session.data}
            nodes={nodes}
            current={current}
            onClose={() => setPaletteOpen(false)}
            onLogout={logout}
          />
        )}
      </StatsProvider>
    </QueryClientProvider>
  )
}
