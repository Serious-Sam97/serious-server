import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Link, useOutletContext } from 'react-router'
import { anyProject, api } from '../api/client'
import type { Project, ServiceStatus } from '../api/client'
import TimeChart from '../components/TimeChart'
import { useSession } from '../components/Layout'
import type { ChromeContext } from '../components/Layout'
import { useFullStats, useStats } from '../lib/stats'
import { fmtBytes, fmtRate, fmtUptime, level, levelText } from '../lib/format'
import type { Level } from '../lib/format'
import { currentNode, nodePath } from '../lib/node'
import { toMarkers } from '../lib/events'
import type { FleetEvent } from '../lib/events'

/** Live windows replay the in-memory 2 s samples; history windows come from
 *  ClickHouse (1-minute rollups: average + peak). */
const WINDOWS = [
  { key: '5m', points: 150, hours: 0 },
  { key: '15m', points: 450, hours: 0 },
  { key: '30m', points: 900, hours: 0 },
  { key: '6h', points: 0, hours: 6 },
  { key: '24h', points: 0, hours: 24 },
  { key: '7d', points: 0, hours: 168 },
  { key: '30d', points: 0, hours: 720 },
] as const
type WindowKey = (typeof WINDOWS)[number]['key']
const S3 = '#f87171'

/** `[x, cpu_avg, cpu_max, mem_avg, mem_max, rx_avg, tx_avg, load_avg, disk_r_avg, disk_w_avg,
 *   disk_used, disk_total, swap_used, swap_total, mem_total, rx_max, tx_max]` */
type HistoryRow = number[]

/** Inline sparkline scaled to its own peak (title shows the peak). */
function MiniSpark({ values }: { values: number[] }) {
  if (values.length < 2) return <span className="text-zinc-600">collecting…</span>
  const peak = Math.max(...values, 0.1)
  const pts = values.map((v, i) => `${(i / (values.length - 1)) * 100},${18 - (v / peak) * 16 - 1}`).join(' ')
  return (
    <svg viewBox="0 0 100 18" preserveAspectRatio="none" className="h-4 w-full text-accent" aria-label={`peak ${peak.toFixed(1)}%`}>
      <title>{`peak ${peak.toFixed(1)}% cpu`}</title>
      <polyline points={pts} fill="none" stroke="currentColor" strokeWidth="1.2" vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

function ContainersPanel({ node }: { node: string }) {
  const q = useQuery({
    queryKey: ['containers', node],
    queryFn: () =>
      api<{
        ts: number | null
        containers: {
          project: string
          service: string
          container: string
          latest: { cpu: number; mem: number; mem_limit: number; rx: number; tx: number } | null
          series: [number, number, number][]
        }[]
      }>(`/fleet/nodes/${encodeURIComponent(node)}/containers?hours=24`),
    refetchInterval: 60_000,
    retry: false,
  })
  if (!q.data || q.data.containers.length === 0) return null
  const rows = [...q.data.containers].sort((a, b) => (b.latest?.mem ?? 0) - (a.latest?.mem ?? 0))
  return (
    <Panel
      title={`CONTAINERS · ${rows.length}`}
      right={<span className="text-zinc-400">last minute · cpu avg, mem · 24 h trend</span>}
    >
      <table className="w-full text-xs">
        <thead className="text-left text-[10px] uppercase tracking-wide text-zinc-500">
          <tr>
            <th className="py-1 pr-2 font-normal">container</th>
            <th className="px-2 py-1 text-right font-normal">cpu</th>
            <th className="px-2 py-1 text-right font-normal">mem</th>
            <th className="px-2 py-1 text-right font-normal">24 h peak</th>
            <th className="w-40 py-1 pl-2 font-normal">cpu · 24 h</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-zinc-800/60">
          {rows.map((c) => {
            const peak = Math.max(0, ...c.series.map((p) => p[2]))
            const memPct = c.latest && c.latest.mem_limit > 0 ? (c.latest.mem / c.latest.mem_limit) * 100 : 0
            return (
              <tr key={c.container} className={c.latest ? 'text-zinc-200' : 'text-zinc-600'}>
                <td className="max-w-56 truncate py-1 pr-2" title={`${c.project}/${c.service}`}>
                  {c.container}
                </td>
                <td className={`px-2 py-1 text-right tabular-nums ${levelText[level(c.latest?.cpu ?? 0, 75, 95)]}`}>
                  {c.latest ? `${c.latest.cpu.toFixed(1)}%` : '–'}
                </td>
                <td className={`px-2 py-1 text-right tabular-nums ${levelText[level(memPct, 80, 92)]}`}>
                  {c.latest ? fmtBytes(c.latest.mem) : 'stopped'}
                </td>
                <td className="px-2 py-1 text-right tabular-nums text-zinc-400">{peak ? fmtBytes(peak) : '–'}</td>
                <td className="py-1 pl-2">
                  <MiniSpark values={c.series.map((p) => p[1])} />
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </Panel>
  )
}

const S1 = 'var(--color-series-1)'
const S2 = 'var(--color-series-2)'

function loadWindow(): WindowKey {
  try {
    const v = localStorage.getItem('ss.system.window')
    const found = WINDOWS.find((w) => w.key === v)
    if (found) return found.key
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
  useFullStats()
  const { chromeHidden, setChromeHidden } = useOutletContext<ChromeContext>()
  const session = useSession()
  const [win, setWin] = useState<WindowKey>(loadWindow)

  const services = useQuery({
    queryKey: ['services'],
    queryFn: () => api<ServiceStatus[]>('/system/services'),
    refetchInterval: 10_000,
  })
  const node = currentNode()
  const winDef = WINDOWS.find((w) => w.key === win)!
  const longWin = winDef.hours > 0
  const hist = useQuery({
    queryKey: ['history', node, winDef.hours],
    queryFn: () =>
      api<{ points: HistoryRow[] }>(`/fleet/nodes/${encodeURIComponent(node)}/history?hours=${winDef.hours}`),
    enabled: longWin,
    refetchInterval: 60_000,
    retry: false,
  })
  const eventsQ = useQuery({
    queryKey: ['events', node, Math.max(winDef.hours, 1)],
    queryFn: () =>
      api<FleetEvent[]>(`/fleet/events?node=${encodeURIComponent(node)}&hours=${Math.max(winDef.hours, 1)}`),
    refetchInterval: 60_000,
    retry: false,
  })
  const markers = toMarkers(eventsQ.data ?? [])
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

  const h = longWin ? (hist.data?.points ?? []) : history.slice(-winDef.points)
  const times = h.map((p) => p[0])
  const col = (i: number) => h.map((p) => p[i])
  // History rows and live points share columns 0 (time) and 1 (cpu); the rest differ.
  const cpuPeak = longWin ? [{ label: 'peak', values: col(2), color: S3, dashed: true }] : []
  const memAvg = longWin ? col(3) : col(2)
  const memPeak = longWin ? [{ label: 'peak', values: col(4), color: S3, dashed: true }] : []
  const rxCol = longWin ? col(5) : col(3)
  const txCol = longWin ? col(6) : col(4)
  const drCol = longWin ? col(8) : col(5)
  const dwCol = longWin ? col(9) : col(6)
  const pct = (a: number, b: number) => (b > 0 ? (a / b) * 100 : null)
  const diskPct = longWin ? h.map((p) => pct(p[10], p[11])) : []
  const swapPct = longWin ? h.map((p) => pct(p[12], p[13])) : []

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
          {longWin
            ? `◷ history · ${win} · avg + peak`
            : stale || !connected
              ? '○ STALE — reconnecting'
              : '● LIVE 2s'}
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
            series={[{ label: 'cpu', values: col(1), color: S1, area: true }, ...cpuPeak]}
            times={times}
            max={100}
            format={(v) => `${v.toFixed(0)}%`}
            markers={markers}
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
            series={[{ label: 'used', values: memAvg, color: S1, area: true }, ...memPeak]}
            times={times}
            max={stats.mem.total}
            format={(v) => fmtBytes(v)}
            markers={markers}
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
              { label: 'rx ↓', values: rxCol, color: S1 },
              { label: 'tx ↑', values: txCol, color: S2 },
            ]}
            times={times}
            format={fmtRate}
            markers={markers}
          />
        </Panel>

        <Panel title="DISK I/O">
          <div className="mb-2 flex items-end gap-4">
            <Big value={`R ${fmtRate(io.read_rate)}`} />
            <span className="pb-1 text-lg tabular-nums text-zinc-300">W {fmtRate(io.write_rate)}</span>
          </div>
          <TimeChart
            series={[
              { label: 'read', values: drCol, color: S1 },
              { label: 'write', values: dwCol, color: S2 },
            ]}
            times={times}
            format={fmtRate}
            markers={markers}
          />
        </Panel>
      </div>

      {longWin && hist.isError && (
        <div className="border border-zinc-800 px-3 py-2 text-xs text-zinc-400">
          History windows need ClickHouse on the master (SS_CLICKHOUSE_URL).
        </div>
      )}
      {longWin && h.length > 0 && (
        <Panel title="DISK SPACE · SWAP" right={<span className="text-zinc-400">% used (peak per bucket)</span>}>
          <TimeChart
            series={[
              { label: 'disk /', values: diskPct, color: S1, area: true },
              { label: 'swap', values: swapPct, color: S2 },
            ]}
            times={times}
            max={100}
            format={(v) => `${v.toFixed(1)}%`}
            height={110}
            markers={markers}
          />
        </Panel>
      )}

      <ContainersPanel node={node} />

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
            <span className="text-zinc-500">sampling…</span>
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
                <Link to={nodePath('/projects')} className="text-accent hover:text-accent-hi">
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
                        to={nodePath(`/projects/${encodeURIComponent(p.name)}`)}
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
