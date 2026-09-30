import { useEffect, useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Navigate, NavLink, Outlet, useNavigate } from 'react-router'
import { anyProject, api } from '../api/client'
import type { ServiceStatus, SessionInfo } from '../api/client'
import { StatsProvider, useStats } from '../lib/stats'
import { fmtBytes, fmtRate, fmtUptime, level, levelText } from '../lib/format'
import CommandPalette from './CommandPalette'

export function useSession() {
  return useQuery({
    queryKey: ['session'],
    queryFn: () => api<SessionInfo>('/auth/session'),
  })
}

export interface ChromeContext {
  chromeHidden: boolean
  setChromeHidden: (hidden: boolean) => void
}

export const canSystem = (s: SessionInfo | undefined) =>
  s?.role === 'admin' || !!s?.permissions.system

export function navFor(s: SessionInfo) {
  const admin = s.role === 'admin'
  return [
    { to: '/projects', label: 'projects', show: anyProject(s, 'view') },
    { to: '/system', label: 'system', show: canSystem(s) },
    { to: '/terminal', label: 'term', show: admin },
    { to: '/files', label: 'files', show: anyProject(s, 'files') },
    { to: '/users', label: 'users', show: admin },
    { to: '/audit', label: 'audit', show: admin },
  ].filter((n) => n.show)
}

/// Index route: land on the first page this user can actually see.
export function Home() {
  const session = useSession()
  if (!session.data) return null
  const nav = navFor(session.data)
  if (nav.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-zinc-500">
        No permissions granted yet — ask the admin.
      </div>
    )
  }
  return <Navigate to={nav[0].to} replace />
}

const SPARK = '▁▂▃▄▅▆▇█'

function StatusLine({ username, showSystem }: { username: string | null; showSystem: boolean }) {
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

  return (
    <footer className="flex h-7 shrink-0 items-center gap-5 overflow-hidden whitespace-nowrap bg-accent px-4 text-xs font-medium text-zinc-950">
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
        <span>serious-server</span>
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

export default function Layout() {
  const navigate = useNavigate()
  const session = useSession()
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [chromeHidden, setChromeHidden] = useState(false)

  const nav = useMemo(() => (session.data ? navFor(session.data) : []), [session.data])

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
      if (i >= 1 && i <= nav.length) navigate(nav[i - 1].to)
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

  const showSystem = canSystem(session.data)
  const username = session.data?.username ?? null

  return (
    <StatsProvider enabled={showSystem}>
      <div className="flex h-screen flex-col">
        {!chromeHidden && (
          <header className="flex h-11 shrink-0 items-center gap-4 border-b border-zinc-800 px-4">
            <span className="font-bold text-accent">serious://</span>
            {showSystem && <CpuBadge />}
            <nav className="ml-2 flex h-full items-stretch gap-0.5">
              {nav.map(({ to, label }, i) => (
                <NavLink
                  key={to}
                  to={to}
                  className={({ isActive }) =>
                    `flex items-center border-t-2 px-3 transition-colors ${
                      isActive
                        ? 'border-accent bg-zinc-900 text-zinc-50'
                        : 'border-transparent text-zinc-500 hover:text-zinc-200'
                    }`
                  }
                >
                  {i + 1}:{label}
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
        <main className="min-h-0 flex-1 overflow-auto">
          <Outlet context={{ chromeHidden, setChromeHidden } satisfies ChromeContext} />
        </main>
        {!chromeHidden && <StatusLine username={username} showSystem={showSystem} />}
      </div>
      {paletteOpen && session.data && (
        <CommandPalette session={session.data} onClose={() => setPaletteOpen(false)} onLogout={logout} />
      )}
    </StatsProvider>
  )
}
