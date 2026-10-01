import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Link, useSearchParams } from 'react-router'
import { api } from '../api/client'
import { useFleetNodes } from '../components/Layout'
import { describe, tone, toneText } from '../lib/events'
import type { FleetEvent } from '../lib/events'
import { nodePath } from '../lib/node'

const WINDOWS = [
  { key: '24h', hours: 24 },
  { key: '7d', hours: 168 },
  { key: '30d', hours: 720 },
]

const KINDS = [
  { key: '', label: 'all' },
  { key: 'docker', label: 'containers' },
  { key: 'backup', label: 'backups ok' },
  { key: 'backup_failed', label: 'backups failed' },
  { key: 'alert', label: 'alerts' },
  { key: 'offline', label: 'offline' },
  { key: 'online', label: 'online' },
]

const chip = (on: boolean) =>
  `px-2 py-0.5 text-xs ${on ? 'bg-accent text-zinc-950' : 'border border-zinc-700 text-zinc-400 hover:bg-zinc-800'}`

/** Everything that happened across the fleet: container lifecycle, backups,
 *  alerts and links coming and going — from ClickHouse (1 year kept). */
export default function Events() {
  const [params, setParams] = useSearchParams()
  const node = params.get('node') ?? ''
  const [win, setWin] = useState('24h')
  const [kind, setKind] = useState('')
  const [hideStarts, setHideStarts] = useState(true)
  const nodes = useFleetNodes()
  const hours = WINDOWS.find((w) => w.key === win)!.hours

  const q = useQuery({
    queryKey: ['events-page', node, hours, kind],
    queryFn: () =>
      api<FleetEvent[]>(
        `/fleet/events?hours=${hours}${node ? `&node=${encodeURIComponent(node)}` : ''}${kind ? `&kind=${kind}` : ''}`,
      ),
    refetchInterval: 30_000,
  })

  const shown = (q.data ?? []).filter(
    (e) => !(hideStarts && e.kind === 'docker' && ['start', 'health_status'].includes(String(e.detail.action))),
  )
  const byDay = new Map<string, FleetEvent[]>()
  for (const e of shown) {
    const day = new Date(e.ts * 1000).toLocaleDateString([], { weekday: 'short', day: 'numeric', month: 'short' })
    byDay.set(day, [...(byDay.get(day) ?? []), e])
  }

  return (
    <div className="space-y-4 p-4">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-lg font-bold text-zinc-100">events</h1>
        <select
          value={node}
          onChange={(e) => setParams(e.target.value ? { node: e.target.value } : {})}
          className="h-7 border border-zinc-700 bg-zinc-900 px-2 text-xs"
        >
          <option value="">all nodes</option>
          {(nodes.data ?? []).map((n) => (
            <option key={n.name} value={n.name}>
              {n.name}
            </option>
          ))}
        </select>
        <div className="flex gap-1">
          {WINDOWS.map((w) => (
            <button key={w.key} type="button" className={chip(win === w.key)} onClick={() => setWin(w.key)}>
              {w.key}
            </button>
          ))}
        </div>
        <div className="flex flex-wrap gap-1">
          {KINDS.map((k) => (
            <button key={k.key} type="button" className={chip(kind === k.key)} onClick={() => setKind(k.key)}>
              {k.label}
            </button>
          ))}
        </div>
        <label className="flex items-center gap-1.5 text-xs text-zinc-400">
          <input type="checkbox" checked={hideStarts} onChange={() => setHideStarts(!hideStarts)} />
          hide routine starts
        </label>
        <span className="text-xs text-zinc-500">{shown.length} events</span>
      </div>

      {q.isLoading && <div className="text-zinc-500">loading…</div>}
      {q.data && shown.length === 0 && <div className="text-zinc-500">nothing in this window.</div>}

      {[...byDay.entries()].map(([day, list]) => (
        <div key={day}>
          <div className="mb-1 text-[11px] uppercase tracking-wide text-zinc-500">{day}</div>
          <div className="divide-y divide-zinc-800/60 border border-zinc-800">
            {list.map((e, i) => (
              <div key={`${e.ts}-${i}`} className="flex items-baseline gap-3 px-3 py-1.5 text-sm">
                <span className="w-16 shrink-0 tabular-nums text-xs text-zinc-500">
                  {new Date(e.ts * 1000).toLocaleTimeString([], { hour12: false })}
                </span>
                <Link
                  to={nodePath('/system', e.node)}
                  className="w-24 shrink-0 truncate text-xs text-zinc-400 hover:text-zinc-200"
                >
                  {e.node}
                </Link>
                <span className={`truncate ${toneText[tone(e)]}`}>{describe(e)}</span>
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  )
}
