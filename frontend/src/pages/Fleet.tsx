import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api, ApiError } from '../api/client'
import { canSystem, useFleetNodes, useSession } from '../components/Layout'
import { fmtBytes, fmtRate, fmtUptime, level, levelText } from '../lib/format'
import { nodeAllows, nodePath } from '../lib/node'
import TimeChart from '../components/TimeChart'
import type { FleetNode } from '../lib/node'

const ENV_COLORS = ['#ef4444', '#f59e0b', '#22c55e', '#3b82f6', '#a855f7']

function Spark({ values, max = 100, className = '' }: { values: number[]; max?: number; className?: string }) {
  if (values.length < 2) return <div className={`h-8 ${className}`} />
  const w = 180
  const h = 32
  const pts = values
    .map((v, i) => `${(i / (values.length - 1)) * w},${h - (Math.min(v, max) / max) * (h - 2) - 1}`)
    .join(' ')
  return (
    <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" className={`h-8 w-full ${className}`}>
      <polyline points={pts} fill="none" stroke="currentColor" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
    </svg>
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

/** 24 h CPU from ClickHouse (absent when the master has no ClickHouse). */
function DayCpu({ node }: { node: string }) {
  const q = useQuery({
    queryKey: ['fleet-history', node],
    queryFn: () => api<{ points: number[][] }>(`/fleet/nodes/${encodeURIComponent(node)}/history?hours=24`),
    refetchInterval: 5 * 60_000,
    retry: false,
  })
  if (!q.data || q.data.points.length < 2) return null
  return (
    <div className="mt-2">
      <div className="text-[10px] uppercase tracking-wide text-zinc-500">cpu · 24 h</div>
      <Spark values={q.data.points.map((p) => p[1])} className="text-zinc-400" />
    </div>
  )
}

/** Last 7 days at a glance: reporting uptime, container churn, backups. */
function WeekSummary({ node }: { node: string }) {
  const q = useQuery({
    queryKey: ['node-summary', node],
    queryFn: () =>
      api<{
        uptime_pct: number | null
        containers: { container: string; restarts: number; dies: number; ooms: number }[]
        backups: { ok: number; failed: number }
      }>(`/fleet/nodes/${encodeURIComponent(node)}/summary?days=7`),
    refetchInterval: 5 * 60_000,
    retry: false,
  })
  const d = q.data
  if (!d) return null
  const restarts = d.containers.reduce((a, c) => a + c.restarts, 0)
  const dies = d.containers.reduce((a, c) => a + c.dies, 0)
  const ooms = d.containers.reduce((a, c) => a + c.ooms, 0)
  const worst = d.containers[0]
  return (
    <div className="flex flex-wrap gap-x-3 text-[11px] text-zinc-500" title={worst ? `most: ${worst.container}` : ''}>
      <span>7 d</span>
      {d.uptime_pct != null && (
        <span className={d.uptime_pct < 99 ? 'text-amber-400' : 'text-zinc-300'}>{d.uptime_pct.toFixed(2)}% up</span>
      )}
      <span className={dies > 0 ? 'text-amber-400' : ''}>
        {restarts} restarts · {dies} dies{ooms > 0 && <span className="text-red-400"> · {ooms} OOM</span>}
      </span>
      {(d.backups.ok > 0 || d.backups.failed > 0) && (
        <span className={d.backups.failed > 0 ? 'text-red-400' : ''}>
          backups {d.backups.ok} ok{d.backups.failed > 0 && ` / ${d.backups.failed} failed`}
        </span>
      )}
    </div>
  )
}

/** One metric for every node on the same axis. */
function Compare() {
  const [metric, setMetric] = useState<'cpu' | 'mem'>('cpu')
  const [hours, setHours] = useState(24)
  const q = useQuery({
    queryKey: ['compare', metric, hours],
    queryFn: () =>
      api<{ times: number[]; series: { node: string; color: string | null; values: (number | null)[] }[] }>(
        `/fleet/compare?hours=${hours}&metric=${metric}`,
      ),
    refetchInterval: 60_000,
    retry: false,
  })
  if (!q.data || q.data.series.length < 2) return null
  const chip = (on: boolean) => `px-2 py-0.5 ${on ? 'bg-accent text-zinc-950' : 'hover:bg-zinc-800'}`
  return (
    <div className="border border-zinc-800 bg-zinc-950 p-3">
      <div className="mb-2 flex items-center gap-3 text-xs">
        <span className="font-bold text-zinc-100">compare</span>
        <div className="flex border border-zinc-700">
          <button type="button" className={chip(metric === 'cpu')} onClick={() => setMetric('cpu')}>cpu %</button>
          <button type="button" className={chip(metric === 'mem')} onClick={() => setMetric('mem')}>mem %</button>
        </div>
        <div className="flex border border-zinc-700">
          <button type="button" className={chip(hours === 24)} onClick={() => setHours(24)}>24h</button>
          <button type="button" className={chip(hours === 168)} onClick={() => setHours(168)}>7d</button>
        </div>
      </div>
      <TimeChart
        series={q.data.series.map((s) => ({
          label: s.node,
          values: s.values,
          color: s.color ?? 'var(--color-accent)',
        }))}
        times={q.data.times}
        max={100}
        format={(v) => `${v.toFixed(1)}%`}
        height={150}
      />
    </div>
  )
}

function NodeCard({ n, admin, onRevoke }: { n: FleetNode; admin: boolean; onRevoke: (n: FleetNode) => void }) {
  const last = n.last
  const s = n.summary
  const cpu = last?.[1] ?? null
  const memFrac = last && s?.mem_total ? last[2] / s.mem_total : null
  const diskFrac = s?.disk_total ? s.disk_used / s.disk_total : null
  const target = nodeAllows(n, 'system') ? '/system' : '/projects'
  const docker = (n.events ?? []).filter((e) => e.kind === 'docker').slice(0, 3)

  return (
    <div
      className="flex flex-col border border-zinc-800 bg-zinc-950"
      style={{ borderTopColor: n.local ? undefined : (n.color ?? undefined), borderTopWidth: n.local ? 1 : 3 }}
    >
      <div className="flex items-center gap-2 border-b border-zinc-800 px-3 py-2">
        <span className={n.online ? 'text-emerald-400' : 'text-zinc-600'}>●</span>
        <Link to={nodePath(target, n.name)} className="font-bold text-zinc-100 hover:text-accent">
          {n.name}
        </Link>
        {n.local ? (
          <span className="text-[10px] text-zinc-500">master</span>
        ) : (
          <span className="px-1 text-[10px] font-bold uppercase text-zinc-950" style={{ background: n.color ?? '#a1a1aa' }}>
            {n.env}
          </span>
        )}
        <span className="flex-1 truncate text-right text-xs text-zinc-500">{n.hostname}</span>
      </div>

      {n.online && last ? (
        <div className="grid grid-cols-4 gap-2 px-3 pt-3 text-xs">
          <div>
            <div className="text-zinc-500">cpu</div>
            <div className={`text-lg tabular-nums ${levelText[level(cpu ?? 0, 75, 90)]}`}>{cpu?.toFixed(0)}%</div>
          </div>
          <div>
            <div className="text-zinc-500">mem</div>
            <div className={`text-lg tabular-nums ${levelText[level((memFrac ?? 0) * 100, 80, 92)]}`}>
              {memFrac != null ? `${(memFrac * 100).toFixed(0)}%` : '–'}
            </div>
          </div>
          <div>
            <div className="text-zinc-500">disk</div>
            <div className={`text-lg tabular-nums ${levelText[level((diskFrac ?? 0) * 100, 85, 95)]}`}>
              {diskFrac != null ? `${(diskFrac * 100).toFixed(0)}%` : '–'}
            </div>
          </div>
          <div>
            <div className="text-zinc-500">load</div>
            <div className="text-lg tabular-nums text-zinc-200">{last[7].toFixed(2)}</div>
          </div>
        </div>
      ) : (
        <div className="px-3 pt-3 text-sm text-red-300">offline · last seen {ago(n.last_seen)}</div>
      )}

      <div className="px-3 pt-2">
        <div className="text-[10px] uppercase tracking-wide text-zinc-500">cpu · live</div>
        <Spark values={n.spark} className={n.online ? 'text-accent' : 'text-zinc-700'} />
        <DayCpu node={n.name} />
      </div>

      <div className="mt-auto space-y-1 px-3 py-2 text-xs text-zinc-500">
        <WeekSummary node={n.name} />
        {n.online && last && s && (
          <div className="tabular-nums">
            {fmtBytes(last[2])}/{fmtBytes(s.mem_total)} · ↓{fmtRate(last[3])} ↑{fmtRate(last[4])} · up{' '}
            {fmtUptime(s.uptime)}
          </div>
        )}
        {docker.map((e, i) => (
          <div key={i} className="truncate">
            {new Date(e.ts * 1000).toLocaleTimeString([], { hour12: false })}{' '}
            <span className="text-zinc-300">{String(e.detail.action ?? '')}</span> {String(e.detail.name ?? '')}
          </div>
        ))}
        <div className="flex items-center gap-3 pt-1">
          {!n.local && <span>v{n.version ?? '?'}</span>}
          {!n.local && n.allow && <span className="truncate" title="SS_AGENT_ALLOW">{n.allow.join(' ')}</span>}
          <div className="flex-1" />
          {admin && !n.local && (
            <button type="button" onClick={() => onRevoke(n)} className="text-red-400 hover:text-red-300">
              revoke
            </button>
          )}
        </div>
      </div>
    </div>
  )
}

function AddNode() {
  const qc = useQueryClient()
  const [name, setName] = useState('')
  const [env, setEnv] = useState('prod')
  const [color, setColor] = useState(ENV_COLORS[0])
  const create = useMutation({
    mutationFn: () =>
      api<{ token: string; name: string; expires_at: number }>('/admin/fleet/tokens', {
        method: 'POST',
        body: { name, env, color },
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['fleet-nodes'] }),
  })
  const t = create.data
  const master = `https://fleet.${location.hostname.split('.').slice(-2).join('.')}`

  return (
    <div className="border border-zinc-800 bg-zinc-950 p-4">
      <h2 className="mb-3 font-bold text-zinc-100">add a node</h2>
      <form
        className="flex flex-wrap items-end gap-3 text-sm"
        onSubmit={(e) => {
          e.preventDefault()
          create.mutate()
        }}
      >
        <label className="flex flex-col gap-1">
          <span className="text-xs text-zinc-500">name</span>
          <input
            value={name}
            onChange={(e) => setName(e.target.value.toLowerCase())}
            placeholder="do-1"
            className="h-8 w-40 border border-zinc-700 bg-zinc-900 px-2"
          />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-xs text-zinc-500">environment</span>
          <input
            value={env}
            onChange={(e) => setEnv(e.target.value)}
            className="h-8 w-28 border border-zinc-700 bg-zinc-900 px-2"
          />
        </label>
        <div className="flex flex-col gap-1">
          <span className="text-xs text-zinc-500">colour</span>
          <div className="flex h-8 items-center gap-1">
            {ENV_COLORS.map((c) => (
              <button
                key={c}
                type="button"
                aria-label={c}
                onClick={() => setColor(c)}
                className={`h-6 w-6 ${color === c ? 'ring-2 ring-zinc-100' : ''}`}
                style={{ background: c }}
              />
            ))}
          </div>
        </div>
        <button
          type="submit"
          disabled={!name || create.isPending}
          className="h-8 bg-accent px-4 font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-40"
        >
          create join token
        </button>
      </form>
      {create.error && <p className="mt-2 text-sm text-red-400">{(create.error as ApiError).message}</p>}
      {t && (
        <div className="mt-4 space-y-2 text-sm">
          <p className="text-zinc-300">
            Single-use token for <b>{t.name}</b>, valid until{' '}
            {new Date(t.expires_at * 1000).toLocaleTimeString([], { hour12: false })}. It is shown once. Put these
            in <code>deploy/agent/.env</code> on the droplet and run <code>docker compose up -d</code>:
          </p>
          <pre className="overflow-auto border border-zinc-800 bg-zinc-900 p-3 text-xs text-zinc-200">
            {`SS_MASTER_URL=${master}
SS_JOIN_TOKEN=${t.token}
SS_CF_ACCESS_CLIENT_ID=<service token id>
SS_CF_ACCESS_CLIENT_SECRET=<service token secret>`}
          </pre>
          <p className="text-xs text-zinc-500">
            After the first connection the agent keeps its own credentials in its data dir; the token can be removed.
          </p>
        </div>
      )}
    </div>
  )
}

interface Alert {
  key: string
  severity: 'critical' | 'warning'
  node: string
  title: string
  detail: string
  since: number
}

function Alerts() {
  const q = useQuery({
    queryKey: ['fleet-alerts'],
    queryFn: () => api<Alert[]>('/fleet/alerts'),
    refetchInterval: 30_000,
  })
  const alerts = q.data ?? []
  if (alerts.length === 0) return null
  return (
    <div className="border border-red-900 bg-red-950/40">
      {alerts.map((a) => (
        <div key={a.key} className="flex items-baseline gap-3 border-b border-red-900/50 px-3 py-1.5 text-sm last:border-0">
          <span className={a.severity === 'critical' ? 'font-bold text-red-400' : 'font-bold text-amber-400'}>
            {a.severity === 'critical' ? '▲' : '●'}
          </span>
          <Link to={nodePath('/projects', a.node)} className="text-zinc-400 hover:text-zinc-200">
            {a.node}
          </Link>
          <span className="text-zinc-100">{a.title}</span>
          <span className="truncate text-xs text-zinc-500">{a.detail}</span>
          <div className="flex-1" />
          <span className="text-xs text-zinc-500">since {ago(a.since)}</span>
        </div>
      ))}
    </div>
  )
}

export default function Fleet() {
  const session = useSession()
  const qc = useQueryClient()
  const nodes = useFleetNodes()
  const admin = session.data?.role === 'admin'
  const revoke = useMutation({
    mutationFn: (name: string) => api(`/admin/fleet/nodes/${encodeURIComponent(name)}`, { method: 'DELETE' }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['fleet-nodes'] }),
  })

  function onRevoke(n: FleetNode) {
    const typed = prompt(
      `Revoke ${n.name} (${n.env.toUpperCase()})? Its link drops now and it can never reconnect with its current credentials.\n\nType the node name to confirm:`,
    )
    if (typed === n.name) revoke.mutate(n.name)
  }

  const list = (nodes.data ?? []).filter((n) => !n.local || canSystem(session.data))
  const online = list.filter((n) => n.online).length

  return (
    <div className="space-y-4 p-4">
      <div className="flex items-baseline gap-3">
        <h1 className="text-lg font-bold text-zinc-100">fleet</h1>
        <span className="text-sm text-zinc-500">
          {online}/{list.length} online
        </span>
      </div>
      <Alerts />
      <Compare />
      {nodes.isLoading && <div className="text-zinc-500">loading…</div>}
      <div className="grid gap-3 [grid-template-columns:repeat(auto-fill,minmax(300px,1fr))]">
        {list.map((n) => (
          <NodeCard key={n.name} n={n} admin={admin} onRevoke={onRevoke} />
        ))}
      </div>
      {admin && <AddNode />}
    </div>
  )
}
