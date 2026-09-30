import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Link, useOutletContext } from 'react-router'
import { anyProject, api } from '../api/client'
import type { Project, ServiceStatus } from '../api/client'
import TimeChart from '../components/TimeChart'
import { useSession } from '../components/Layout'
import type { ChromeContext } from '../components/Layout'
import { useStats } from '../lib/stats'
import { fmtBytes, fmtRate, fmtUptime, level, levelText } from '../lib/format'
import type { Level } from '../lib/format'

const WINDOWS = [
  { key: '5m', points: 150 },
  { key: '15m', points: 450 },
  { key: '30m', points: 900 },
] as const
type WindowKey = (typeof WINDOWS)[number]['key']

const S1 = 'var(--color-series-1)'
const S2 = 'var(--color-series-2)'

function loadWindow(): WindowKey {
  try {
    const v = localStorage.getItem('ss.system.window')
    if (v === '5m' || v === '15m' || v === '30m') return v
  } catch {
    /* storage unavailable */
  }
  return '15m'
}

function Panel({
  title,
  right,
  children,
  className = '',
}: {
  title: string
  right?: React.ReactNode
  children: React.ReactNode
  className?: string
}) {
  return (
    <section className={`flex min-w-0 flex-col border border-zinc-800 bg-zinc-900 ${className}`}>
      <header className="flex items-center gap-2 border-b border-zinc-800 px-3 py-1.5 text-[11px] tracking-widest text-zinc-500">
        <span>{title}</span>
        <span className="ml-auto tracking-normal">{right}</span>
      </header>
      <div className="flex min-h-0 flex-1 flex-col p-3">{children}</div>
    </section>
  )
}

function Big({ value, unit, lvl = 'ok' }: { value: string; unit?: string; lvl?: Level }) {
  return (
    <div className={`text-3xl font-bold tabular-nums ${levelText[lvl]}`}>
      {value}
      {unit && <span className="ml-1 text-base font-normal text-zinc-500">{unit}</span>}
      {lvl !== 'ok' && (
        <span className="ml-2 align-middle text-xs font-medium">
          {lvl === 'crit' ? '▲ CRIT' : '△ HIGH'}
        </span>
      )}
    </div>
  )
}

function Bar({ frac, lvl = 'ok' }: { frac: number; lvl?: Level }) {
  const color = lvl === 'crit' ? 'bg-red-400' : lvl === 'warn' ? 'bg-amber-400' : 'bg-zinc-400'
  return (
    <div className="h-1.5 bg-zinc-800">
      <div className={`h-full ${color}`} style={{ width: `${Math.min(frac, 1) * 100}%` }} />
    </div>
  )
}

export default function Dashboard() {
  const { stats, history, connected, stale } = useStats()
  const { chromeHidden, setChromeHidden } = useOutletContext<ChromeContext>()
  const session = useSession()
  const [win, setWin] = useState<WindowKey>(loadWindow)

  const services = useQuery({
    queryKey: ['services'],
    queryFn: () => api<ServiceStatus[]>('/system/services'),
    refetchInterval: 10_000,
  })
  const showProjects = anyProject(session.data, 'view')
  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: () => api<Project[]>('/projects'),
    refetchInterval: 15_000,
    enabled: showProjects,
  })

  useEffect(() => {
    try {
      localStorage.setItem('ss.system.window', win)
    } catch {
      /* storage unavailable */
    }
  }, [win])

  // `f` toggles focus mode (no app chrome) — meant for a dedicated monitor.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      const t = e.target as HTMLElement
      if (t.closest('input, textarea, select, [contenteditable]')) return
      if (e.metaKey || e.ctrlKey || e.altKey) return
      if (e.key === 'f') setChromeHidden(!chromeHidden)
      else if (e.key === 'Escape' && chromeHidden) setChromeHidden(false)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [chromeHidden, setChromeHidden])

  // Leave focus mode when navigating away.
  useEffect(() => () => setChromeHidden(false), [setChromeHidden])

  // The tab title doubles as a tiny monitor when the window is backgrounded.
  useEffect(() => {
    if (!stats) return
    const host = stats.host?.hostname ?? 'serious-server'
    document.title = `${stale ? '⚠ ' : ''}${stats.cpu.total.toFixed(0)}% · ${host}`
    return () => {
      document.title = 'serious-server'
    }
  }, [stats, stale])

  if (!stats) {
    return (
      <div className="flex h-full items-center justify-center text-zinc-500">
        {connected ? 'waiting for first sample…' : 'connecting to /ws/system…'}
      </div>
    )
  }

  const n = WINDOWS.find((w) => w.key === win)!.points
  const h = history.slice(-n)
  const times = h.map((p) => p[0])
  const col = (i: number) => h.map((p) => p[i])

  const host = stats.host
  const cores = stats.cpu.per_core.length
  const cpuLvl = level(stats.cpu.total, 75, 90)
  const memFrac = stats.mem.used / stats.mem.total
  const memLvl = level(memFrac * 100, 80, 92)
  const swapLvl =
    stats.mem.swap_total > 0 ? level((stats.mem.swap_used / stats.mem.swap_total) * 100, 50, 90) : 'ok'
  const temp = stats.cpu.temp ?? null
  const tempLvl = temp === null ? 'ok' : level(temp, 80, 90)
  const loadLvl = level(stats.load[0] / Math.max(cores, 1), 1, 1.5)
  const rx = stats.net.reduce((a, x) => a + x.rx_rate, 0)
  const tx = stats.net.reduce((a, x) => a + x.tx_rate, 0)
  const io = stats.disk_io ?? { read_rate: 0, write_rate: 0 }
  const containers = projects.data?.reduce(
    (a, p) => ({ running: a.running + p.running, total: a.total + p.total }),
    { running: 0, total: 0 },
  )
  const attention = projects.data?.filter((p) => p.status === 'partial' || p.status === 'stopped')

  return (
    <div className="flex min-h-full flex-col gap-3 p-3">
      {/* host strip */}
      <div className="flex flex-wrap items-center gap-x-5 gap-y-1 px-1 text-xs text-zinc-500">
        <span className="text-sm font-bold text-zinc-100">{host?.hostname ?? 'host'}</span>
        {host?.os && <span>{host.os}</span>}
        {host?.kernel && <span>kernel {host.kernel}</span>}
        {host?.cpu_brand && (
          <span>
            {host.cpu_brand} · {host.physical_cores ?? '?'}c/{host.cores}t
          </span>
        )}
        <span>up {fmtUptime(stats.uptime)}</span>
        <div className="flex-1" />
        <span
          className={`flex items-center gap-1.5 ${stale || !connected ? 'text-red-400' : 'text-emerald-400'}`}
        >
          {stale || !connected ? '○ STALE — reconnecting' : '● LIVE 2s'}
        </span>
        <div className="flex border border-zinc-700" role="group" aria-label="Chart window">
          {WINDOWS.map((w) => (
            <button
              key={w.key}
              type="button"
              onClick={() => setWin(w.key)}
              aria-pressed={win === w.key}
              className={`px-2 py-0.5 ${win === w.key ? 'bg-accent text-zinc-950' : 'hover:bg-zinc-800'}`}
            >
              {w.key}
            </button>
          ))}
        </div>
        <button
          type="button"
          onClick={() => setChromeHidden(!chromeHidden)}
          className="border border-zinc-700 px-2 py-0.5 hover:bg-zinc-800"
          title="Hide navigation for a dedicated monitor (f / Esc)"
        >
          {chromeHidden ? 'exit focus [esc]' : 'focus [f]'}
        </button>
      </div>

      {stale && (
        <div className="border border-red-400/50 bg-red-400/10 px-3 py-1.5 text-xs text-red-300">
          ▲ No data for {Math.round((Date.now() - stats.ts) / 1000)}s — numbers below are frozen
          at {new Date(stats.ts).toLocaleTimeString([], { hour12: false })}.
        </div>
      )}

      {/* vitals — charts stretch to fill whatever height the monitor has */}
      <div className="grid min-h-[480px] flex-1 grid-cols-1 gap-3 md:grid-cols-2 2xl:min-h-[300px] 2xl:grid-cols-4">
        <Panel
          title="CPU"
          right={
            <span className="text-zinc-400">
              load{' '}
              <span className={levelText[loadLvl]}>
                {stats.load.map((l) => l.toFixed(2)).join(' ')}
              </span>
            </span>
          }
        >
          <div className="mb-2 flex items-end justify-between">
            <Big value={stats.cpu.total.toFixed(0)} unit="%" lvl={cpuLvl} />
            {temp !== null && (
              <span className={`text-lg tabular-nums ${levelText[tempLvl]}`}>
                {temp.toFixed(0)}°C
                {tempLvl !== 'ok' && <span className="ml-1 text-xs">{tempLvl === 'crit' ? '▲' : '△'}</span>}
              </span>
            )}
          </div>
          <TimeChart
            series={[{ label: 'cpu', values: col(1), color: S1, area: true }]}
            times={times}
            max={100}
            format={(v) => `${v.toFixed(0)}%`}
          />
        </Panel>

        <Panel
          title="MEMORY"
          right={
            stats.mem.swap_total > 0 && (
              <span className={swapLvl === 'ok' ? 'text-zinc-400' : levelText[swapLvl]}>
                swap {fmtBytes(stats.mem.swap_used)} / {fmtBytes(stats.mem.swap_total)}
                {swapLvl !== 'ok' && ' ▲'}
              </span>
            )
          }
        >
          <div className="mb-2 flex items-end justify-between">
            <Big value={fmtBytes(stats.mem.used)} unit={`/ ${fmtBytes(stats.mem.total)}`} lvl={memLvl} />
            {stats.mem.available !== undefined && (
              <span className="text-zinc-400">{fmtBytes(stats.mem.available)} avail</span>
            )}
          </div>
          <TimeChart
            series={[{ label: 'used', values: col(2), color: S1, area: true }]}
            times={times}
            max={stats.mem.total}
            format={(v) => fmtBytes(v)}
          />
        </Panel>

        <Panel
          title="NETWORK"
          right={<span className="text-zinc-400">{stats.net.map((x) => x.iface).join(' ')}</span>}
        >
          <div className="mb-2 flex items-end gap-4">
            <Big value={`↓${fmtRate(rx)}`} />
            <span className="pb-1 text-lg tabular-nums text-zinc-300">↑{fmtRate(tx)}</span>
          </div>
          <TimeChart
            series={[
              { label: 'rx ↓', values: col(3), color: S1 },
              { label: 'tx ↑', values: col(4), color: S2 },
            ]}
            times={times}
            format={fmtRate}
          />
        </Panel>

        <Panel title="DISK I/O">
          <div className="mb-2 flex items-end gap-4">
            <Big value={`R ${fmtRate(io.read_rate)}`} />
            <span className="pb-1 text-lg tabular-nums text-zinc-300">W {fmtRate(io.write_rate)}</span>
          </div>
          <TimeChart
            series={[
              { label: 'read', values: col(5), color: S1 },
              { label: 'write', values: col(6), color: S2 },
            ]}
            times={times}
            format={fmtRate}
          />
        </Panel>
      </div>

      {/* detail */}
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 2xl:grid-cols-4">
        <Panel title={`CORES · ${cores}`} right={<span className="text-zinc-400">% busy</span>}>
          <div className="grid gap-1" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(52px, 1fr))' }}>
            {stats.cpu.per_core.map((c, i) => (
              <div
                key={i}
                className="relative flex h-10 flex-col justify-between border border-zinc-800 px-1.5 py-1 text-[10px]"
                title={`core ${i}: ${c.toFixed(1)}%`}
              >
                <div
                  className="absolute inset-0 bg-accent"
                  style={{ opacity: 0.08 + (Math.min(c, 100) / 100) * 0.6 }}
                />
                <span className="relative text-zinc-400">c{i}</span>
                <span className={`relative text-right text-xs tabular-nums ${c >= 90 ? 'font-bold text-zinc-50' : 'text-zinc-100'}`}>
                  {c.toFixed(0)}
                </span>
              </div>
            ))}
          </div>
        </Panel>

        <Panel
          title="TOP PROCESSES"
          right={stats.processes && <span className="text-zinc-400">{stats.processes.count} total</span>}
        >
          {stats.processes ? (
            <table className="w-full text-xs">
              <thead className="text-left text-[10px] tracking-widest text-zinc-500">
                <tr>
                  <th className="pb-1 font-normal">PID</th>
                  <th className="pb-1 font-normal">NAME</th>
                  <th className="pb-1 text-right font-normal">CPU%</th>
                  <th className="pb-1 text-right font-normal">MEM</th>
                </tr>
              </thead>
              <tbody>
                {stats.processes.top.map((p) => (
                  <tr key={p.pid} className="border-t border-dashed border-zinc-800">
                    <td className="py-1 text-zinc-500 tabular-nums">{p.pid}</td>
                    <td className="max-w-40 truncate py-1">{p.name}</td>
                    <td className={`py-1 text-right tabular-nums ${p.cpu >= 90 ? 'text-amber-400' : ''}`}>
                      {p.cpu.toFixed(1)}
                    </td>
                    <td className="py-1 text-right text-zinc-400 tabular-nums">{fmtBytes(p.mem)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <span className="text-zinc-500">not reported by this server version</span>
          )}
        </Panel>

        <Panel title="DISKS">
          <div className="flex flex-col gap-3">
            {stats.disks.map((d) => {
              const frac = d.used / d.total
              const lvl = level(frac * 100, 85, 95)
              return (
                <div key={d.mount} className="flex flex-col gap-1">
                  <div className="flex justify-between gap-2 text-xs">
                    <span className="truncate">{d.mount}</span>
                    <span className={`shrink-0 tabular-nums ${lvl === 'ok' ? 'text-zinc-400' : levelText[lvl]}`}>
                      {(frac * 100).toFixed(0)}% · {fmtBytes(d.used)}/{fmtBytes(d.total)}
                      {lvl !== 'ok' && ' ▲'}
                    </span>
                  </div>
                  <Bar frac={frac} lvl={lvl} />
                </div>
              )
            })}
          </div>
        </Panel>

        <div className="flex flex-col gap-3">
          <Panel title="SERVICES">
            <div className="flex flex-col gap-1 text-xs">
              {services.data?.length === 0 && <span className="text-zinc-500">none configured</span>}
              {services.data?.map((s) => (
                <div key={s.unit} className="flex justify-between gap-2">
                  <span className="truncate">{s.unit}</span>
                  <span className={s.active_state === 'active' ? 'text-emerald-400' : 'text-red-400'}>
                    {s.active_state === 'active' ? '●' : '○'} {s.active_state}
                    {s.sub_state && s.sub_state !== 'running' ? ` (${s.sub_state})` : ''}
                  </span>
                </div>
              ))}
            </div>
          </Panel>

          {showProjects && containers && (
            <Panel
              title="CONTAINERS"
              right={
                <Link to="/projects" className="text-accent hover:text-accent-hi">
                  open →
                </Link>
              }
            >
              <div className="text-xs">
                <span className="text-lg font-bold tabular-nums">
                  {containers.running}/{containers.total}
                </span>{' '}
                <span className="text-zinc-400">running</span>
                {attention && attention.length > 0 ? (
                  <div className="mt-1 flex flex-col gap-0.5">
                    {attention.map((p) => (
                      <Link
                        key={p.name}
                        to={`/projects/${encodeURIComponent(p.name)}`}
                        className={p.status === 'stopped' ? 'text-red-400' : 'text-amber-400'}
                      >
                        {p.status === 'stopped' ? '○' : '◐'} {p.name} {p.running}/{p.total}
                      </Link>
                    ))}
                  </div>
                ) : (
                  <div className="mt-1 text-emerald-400">● all projects healthy</div>
                )}
              </div>
            </Panel>
          )}

          {stats.temps && stats.temps.length > 0 && (
            <Panel title="SENSORS">
              <div className="flex flex-col gap-1 text-xs">
                {[...stats.temps]
                  .sort((a, b) => b.temp - a.temp)
                  .slice(0, 6)
                  .map((t) => {
                    const lvl = level(t.temp, t.critical ? t.critical - 15 : 80, t.critical ? t.critical - 5 : 90)
                    return (
                      <div key={t.label} className="flex justify-between gap-2">
                        <span className="truncate text-zinc-400">{t.label}</span>
                        <span className={`tabular-nums ${levelText[lvl]}`}>
                          {t.temp.toFixed(0)}°C{lvl !== 'ok' && ' ▲'}
                        </span>
                      </div>
                    )
                  })}
              </div>
            </Panel>
          )}
        </div>
      </div>
    </div>
  )
}
