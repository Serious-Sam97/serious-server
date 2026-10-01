import type { Marker } from '../components/TimeChart'

export interface FleetEvent {
  node: string
  ts: number
  kind: string
  detail: Record<string, unknown>
}

const str = (v: unknown) => (v === null || v === undefined ? '' : String(v))

/** One readable line for an event. */
export function describe(e: FleetEvent): string {
  const d = e.detail
  switch (e.kind) {
    case 'docker':
      return `${str(d.action)} ${str(d.name)}`.trim()
    case 'online':
      return 'came online'
    case 'offline':
      return 'went offline'
    case 'backup':
      return `backup ${str(d.project)}/${str(d.service)} ok · ${str(d.detail)}`
    case 'backup_failed':
      return `backup ${str(d.project)}/${str(d.service)} FAILED · ${str(d.detail)}`
    case 'alert':
      return `${str(d.state) === 'resolved' ? 'resolved' : 'ALERT'}: ${str(d.title)}`
    case 'wal_gap':
      return `WAL chain broken ${str(d.project)}/${str(d.service)} · ${str(d.detail)}`
    default:
      return `${e.kind} ${JSON.stringify(d)}`
  }
}

/** Colour by severity: problems red, recoveries green, the rest quiet. */
export function tone(e: FleetEvent): 'bad' | 'good' | 'neutral' {
  const a = str(e.detail.action)
  if (e.kind === 'offline' || e.kind === 'backup_failed' || e.kind === 'wal_gap') return 'bad'
  if (e.kind === 'alert') return str(e.detail.state) === 'resolved' ? 'good' : 'bad'
  if (e.kind === 'docker' && (a === 'die' || a === 'oom')) return 'bad'
  if (e.kind === 'online' || e.kind === 'backup') return 'good'
  return 'neutral'
}

const TONE_COLOR = { bad: '#f87171', good: '#4ade80', neutral: 'var(--color-zinc-500)' }

export const toneText = { bad: 'text-red-400', good: 'text-emerald-400', neutral: 'text-zinc-400' }

/** Chart markers: the events that explain a spike or a gap. */
export function toMarkers(events: FleetEvent[]): Marker[] {
  return events
    .filter((e) => !(e.kind === 'docker' && ['start', 'health_status'].includes(str(e.detail.action))))
    .map((e) => ({ t: e.ts, label: describe(e), color: TONE_COLOR[tone(e)] }))
}
