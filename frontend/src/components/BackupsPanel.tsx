import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, ApiError, can } from '../api/client'
import type { SessionInfo } from '../api/client'
import { fmtBytes } from '../lib/format'
import { currentNode, onNodeLabel } from '../lib/node'

export interface BackupTarget {
  project: string
  service: string
  container: string
  engine: string
  image: string
  database: string
  running: boolean
}

export interface Policy {
  project: string
  service: string
  engine: string
  mode: 'logical' | 'continuous'
  enabled: boolean
  schedule: string
  utc_offset_min: number
  window: string
  keep_last: number
  keep_daily: number
  keep_weekly: number
  keep_monthly: number
  max_rate_kbps: number
}

interface BackupRow {
  id: number
  project: string
  service: string
  kind: string
  trigger: string
  started_at: number
  finished_at: number | null
  size: number
  status: 'ok' | 'failed'
  error: string
  label: string
  verified: string
  verified_at: number | null
}

interface WalSummary {
  project: string
  service: string
  segments: number
  bytes: number
  first: string | null
  last: string | null
  last_at: number | null
  gap: string | null
}

interface RestoreState {
  stage: string
  ok: boolean | null
  message: string
}

const PRESETS: { label: string; cron: string }[] = [
  { label: 'daily 03:00', cron: '0 3 * * *' },
  { label: 'every 6 h', cron: '0 */6 * * *' },
  { label: 'hourly', cron: '0 * * * *' },
  { label: 'weekly, Sun 03:00', cron: '0 3 * * 0' },
]

const btn = 'h-7 border border-zinc-700 px-2.5 text-xs hover:bg-zinc-800 disabled:opacity-40'
const input = 'h-7 border border-zinc-700 bg-zinc-900 px-2 text-xs'

function when(ts: number | null) {
  if (!ts) return '–'
  return new Date(ts * 1000).toLocaleString([], { hour12: false, dateStyle: 'short', timeStyle: 'short' })
}

function defaults(t: BackupTarget): Policy {
  return {
    project: t.project,
    service: t.service,
    engine: 'postgres',
    mode: 'logical',
    enabled: true,
    schedule: '0 3 * * *',
    utc_offset_min: -new Date().getTimezoneOffset(),
    window: '',
    keep_last: 7,
    keep_daily: 7,
    keep_weekly: 4,
    keep_monthly: 6,
    max_rate_kbps: 0,
  }
}

function PolicyEditor({ node, initial, onSaved }: { node: string; initial: Policy; onSaved: () => void }) {
  const [p, setP] = useState<Policy>(initial)
  useEffect(() => setP(initial), [initial])
  const save = useMutation({
    mutationFn: () => api('/fleet/backups/policy', { method: 'PUT', body: { node, policy: p } }),
    onSuccess: onSaved,
  })
  const num = (k: keyof Policy) => (e: React.ChangeEvent<HTMLInputElement>) =>
    setP({ ...p, [k]: Math.max(0, Number(e.target.value) || 0) })
  const preset = PRESETS.find((x) => x.cron === p.schedule)

  return (
    <div className="space-y-3 border-b border-zinc-800 p-3 text-xs">
      <div className="flex flex-wrap items-center gap-3">
        <label className="flex items-center gap-1.5">
          <input type="checkbox" checked={p.enabled} onChange={() => setP({ ...p, enabled: !p.enabled })} />
          scheduled
        </label>
        <label className="flex items-center gap-1.5">
          mode
          <select
            value={p.mode}
            onChange={(e) => setP({ ...p, mode: e.target.value as Policy['mode'] })}
            className={input}
          >
            <option value="logical">logical (pg_dump)</option>
            <option value="continuous">continuous (WAL + base)</option>
          </select>
        </label>
        <label className="flex items-center gap-1.5">
          when
          <select
            value={preset ? preset.cron : 'custom'}
            onChange={(e) => e.target.value !== 'custom' && setP({ ...p, schedule: e.target.value })}
            className={input}
          >
            {PRESETS.map((x) => (
              <option key={x.cron} value={x.cron}>
                {x.label}
              </option>
            ))}
            <option value="custom">custom cron…</option>
          </select>
        </label>
        <input
          value={p.schedule}
          onChange={(e) => setP({ ...p, schedule: e.target.value })}
          className={`${input} w-32 font-mono`}
          title="minute hour day month weekday"
        />
        <label className="flex items-center gap-1.5" title="the schedule's clock, as UTC offset">
          UTC
          <select
            value={p.utc_offset_min}
            onChange={(e) => setP({ ...p, utc_offset_min: Number(e.target.value) })}
            className={input}
          >
            {Array.from({ length: 27 }, (_, i) => (i - 12) * 60).map((m) => (
              <option key={m} value={m}>
                {m >= 0 ? '+' : '-'}
                {String(Math.abs(m / 60)).padStart(2, '0')}:00
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1.5" title="a missed run is only caught up inside this window">
          window
          <input
            value={p.window}
            onChange={(e) => setP({ ...p, window: e.target.value })}
            placeholder="01:00-05:00"
            className={`${input} w-28`}
          />
        </label>
      </div>
      <div className="flex flex-wrap items-center gap-3">
        <span className="text-zinc-500">keep</span>
        <label className="flex items-center gap-1">
          last <input type="number" min={1} value={p.keep_last} onChange={num('keep_last')} className={`${input} w-14`} />
        </label>
        <label className="flex items-center gap-1">
          daily <input type="number" value={p.keep_daily} onChange={num('keep_daily')} className={`${input} w-14`} />
        </label>
        <label className="flex items-center gap-1">
          weekly <input type="number" value={p.keep_weekly} onChange={num('keep_weekly')} className={`${input} w-14`} />
        </label>
        <label className="flex items-center gap-1">
          monthly <input type="number" value={p.keep_monthly} onChange={num('keep_monthly')} className={`${input} w-14`} />
        </label>
        <label className="flex items-center gap-1" title="0 = unlimited">
          max rate
          <input type="number" value={p.max_rate_kbps} onChange={num('max_rate_kbps')} className={`${input} w-20`} />
          KiB/s
        </label>
        <div className="flex-1" />
        <button
          type="button"
          onClick={() => save.mutate()}
          disabled={save.isPending}
          className="h-7 bg-accent px-3 font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-40"
        >
          save policy
        </button>
      </div>
      {save.error && <p className="text-red-400">{(save.error as ApiError).message}</p>}
      {p.mode === 'continuous' && (
        <p className="text-zinc-500">
          Continuous: WAL streams to the master all the time (data-loss window ≤ 5 min); the schedule above takes
          the base backups.
        </p>
      )}
    </div>
  )
}

function RestoreProgress({ rid, onDone }: { rid: number; onDone: () => void }) {
  const q = useQuery({
    queryKey: ['restore', rid],
    queryFn: () => api<RestoreState>(`/fleet/restores/${rid}`),
    refetchInterval: (query) => (query.state.data?.ok != null ? false : 1500),
  })
  const r = q.data
  useEffect(() => {
    if (r?.ok != null) onDone()
  }, [r?.ok, onDone])
  if (!r) return null
  return (
    <div
      className={`border-b border-zinc-800 px-3 py-2 text-xs ${
        r.ok === false ? 'text-red-300' : r.ok ? 'text-emerald-300' : 'text-zinc-300'
      }`}
    >
      restore: {r.stage}
      {r.ok === false && ` — ${r.message}`}
      {r.ok && ' — the database now holds the restored data (a safety backup of the previous state was taken first)'}
    </div>
  )
}

function TargetCard({ target, session }: { target: BackupTarget; session: SessionInfo }) {
  const node = currentNode()
  const qc = useQueryClient()
  const [editing, setEditing] = useState(false)
  const [rid, setRid] = useState<number | null>(null)
  const key = ['backups', node, target.project, target.service]
  const data = useQuery({
    queryKey: key,
    queryFn: () =>
      api<{ backups: BackupRow[]; policies: { policy: Policy; next_run: number | null }[]; wal: WalSummary[] }>(
        `/fleet/backups?node=${encodeURIComponent(node)}&project=${encodeURIComponent(target.project)}&service=${encodeURIComponent(target.service)}`,
      ),
    refetchInterval: 10_000,
  })
  const refresh = () => qc.invalidateQueries({ queryKey: key })
  const run = useMutation({
    mutationFn: () =>
      api('/fleet/backups/run', { method: 'POST', body: { node, project: target.project, service: target.service } }),
    onSuccess: () => setTimeout(refresh, 3000),
  })
  const del = useMutation({
    mutationFn: (id: number) => api(`/fleet/backups/${id}`, { method: 'DELETE' }),
    onSuccess: refresh,
  })
  const restore = useMutation({
    mutationFn: ({ id, confirm }: { id: number; confirm: string }) =>
      api<{ rid: number }>(`/fleet/backups/${id}/restore`, { method: 'POST', body: { confirm } }),
    onSuccess: (r) => setRid(r.rid),
  })
  const pitr = useMutation({
    mutationFn: ({ target_time, confirm }: { target_time: number | null; confirm: string }) =>
      api<{ rid: number }>('/fleet/backups/pitr', {
        method: 'POST',
        body: { node, project: target.project, service: target.service, target_time, confirm },
      }),
    onSuccess: (r) => setRid(r.rid),
  })
  const verify = useMutation({
    mutationFn: (id: number) => api(`/fleet/backups/${id}/verify`, { method: 'POST' }),
    onSuccess: () => setTimeout(refresh, 5000),
  })
  const [pitrAt, setPitrAt] = useState(() => {
    const d = new Date(Date.now() - new Date().getTimezoneOffset() * 60_000)
    return d.toISOString().slice(0, 16)
  })

  const admin = session.role === 'admin'
  const control = can(session, 'control', target.project)
  const files = can(session, 'files', target.project)
  const entry = data.data?.policies[0]
  const policy = entry?.policy ?? defaults(target)
  const backups = data.data?.backups ?? []
  const lastOk = backups.find((b) => b.status === 'ok')
  const name = `${target.project}/${target.service}`
  const wal = data.data?.wal.find((w) => w.project === target.project && w.service === target.service)
  const continuous = entry?.policy.mode === 'continuous'

  function askPitr(latest: boolean) {
    const at = latest ? null : Math.floor(new Date(pitrAt).getTime() / 1000)
    const label = latest ? 'the latest point the WAL reaches' : new Date(at! * 1000).toLocaleString([], { hour12: false })
    const typed = prompt(
      `Point-in-time restore ${name}${onNodeLabel()} to ${label}?\n\n` +
        `The database is STOPPED, its data replaced by the nearest earlier base backup, and WAL replayed up to that moment. ` +
        `A full copy of the current data is kept in a new docker volume first (automatic rollback if anything fails).\n\n` +
        `Type ${name} to confirm:`,
    )
    if (typed === name) pitr.mutate({ target_time: at, confirm: typed })
  }

  function askRestore(b: BackupRow) {
    const typed = prompt(
      `Restore ${name}${onNodeLabel()} from the backup of ${when(b.started_at)}?\n\n` +
        `This REPLACES the current data in database "${target.database}". A safety backup of the current state is taken first.\n\n` +
        `Type ${name} to confirm:`,
    )
    if (typed === name) restore.mutate({ id: b.id, confirm: typed })
  }

  return (
    <div className="border border-zinc-800">
      <div className="flex flex-wrap items-center gap-3 border-b border-zinc-800 px-3 py-2 text-sm">
        <span className="font-bold text-zinc-100">{target.service}</span>
        <span className="text-xs text-zinc-500">
          {target.engine} · db {target.database} · {target.image}
        </span>
        <div className="flex-1" />
        <span className="text-xs text-zinc-400">
          {entry
            ? policy.enabled
              ? `${policy.mode} · next ${when(entry.next_run)}`
              : 'schedule off'
            : 'no policy yet (defaults shown)'}
          {lastOk && ` · last ok ${when(lastOk.started_at)}`}
        </span>
        {control && (
          <>
            <button type="button" className={btn} onClick={() => setEditing((e) => !e)}>
              policy
            </button>
            <button
              type="button"
              className={btn}
              disabled={run.isPending || !target.running}
              onClick={() => run.mutate()}
              title={target.running ? 'dump now and store it on the master' : 'container is not running'}
            >
              backup now
            </button>
          </>
        )}
      </div>
      {continuous && (
        <div className="flex flex-wrap items-center gap-3 border-b border-zinc-800 px-3 py-2 text-xs">
          <span className={wal?.gap ? 'text-red-300' : 'text-zinc-300'}>
            WAL{' '}
            {wal
              ? `${wal.segments} segments · ${fmtBytes(wal.bytes)} · last ${when(wal.last_at)} · ${
                  wal.gap ? `GAP at ${wal.gap}` : 'chain whole'
                }`
              : 'waiting for the first segment'}
          </span>
          <div className="flex-1" />
          {admin && (
            <>
              <input
                type="datetime-local"
                value={pitrAt}
                onChange={(e) => setPitrAt(e.target.value)}
                className={input}
              />
              <button type="button" className={btn} onClick={() => askPitr(false)} disabled={pitr.isPending}>
                restore to this time
              </button>
              <button type="button" className={btn} onClick={() => askPitr(true)} disabled={pitr.isPending}>
                restore to latest
              </button>
            </>
          )}
        </div>
      )}
      {pitr.error && <p className="px-3 py-1 text-xs text-red-400">{(pitr.error as ApiError).message}</p>}
      {run.error && <p className="px-3 py-1 text-xs text-red-400">{(run.error as ApiError).message}</p>}
      {restore.error && <p className="px-3 py-1 text-xs text-red-400">{(restore.error as ApiError).message}</p>}
      {rid != null && <RestoreProgress rid={rid} onDone={refresh} />}
      {editing && (
        <PolicyEditor
          node={node}
          initial={policy}
          onSaved={() => {
            setEditing(false)
            refresh()
          }}
        />
      )}
      <table className="w-full text-xs">
        <tbody className="divide-y divide-zinc-800/60">
          {backups.slice(0, 50).map((b) => (
            <tr key={b.id} className={b.status === 'failed' ? 'text-red-300' : 'text-zinc-300'}>
              <td className="px-3 py-1.5 tabular-nums">{when(b.started_at)}</td>
              <td className="px-2 py-1.5 text-zinc-500">
                {b.kind === 'base' ? 'base · ' : ''}
                {b.trigger}
              </td>
              <td className="px-2 py-1.5 tabular-nums">{b.status === 'ok' ? fmtBytes(b.size) : ''}</td>
              <td className="max-w-md truncate px-2 py-1.5" title={b.error || b.verified}>
                {b.status === 'ok' ? '✓' : `✗ ${b.error}`}
                {b.verified.startsWith('ok') && <span className="ml-2 text-emerald-400">verified</span>}
                {b.verified.startsWith('failed') && <span className="ml-2 text-red-400">verify failed</span>}
              </td>
              <td className="px-3 py-1.5 text-right">
                {b.status === 'ok' && files && (
                  <a className="mr-3 text-accent hover:text-accent-hi" href={`/api/fleet/backups/${b.id}/download`}>
                    download
                  </a>
                )}
                {b.status === 'ok' && control && (
                  <button
                    type="button"
                    className="mr-3 text-zinc-400 hover:text-zinc-200"
                    onClick={() => verify.mutate(b.id)}
                    title="restore into a throwaway Postgres on the master and check it"
                  >
                    verify
                  </button>
                )}
                {b.status === 'ok' && admin && (
                  <button type="button" className="mr-3 text-amber-300 hover:text-amber-200" onClick={() => askRestore(b)}>
                    restore
                  </button>
                )}
                {admin && (
                  <button
                    type="button"
                    className="text-zinc-500 hover:text-red-400"
                    onClick={() => confirm(`Delete this backup of ${name}${onNodeLabel()}?`) && del.mutate(b.id)}
                  >
                    delete
                  </button>
                )}
              </td>
            </tr>
          ))}
          {backups.length === 0 && (
            <tr>
              <td className="px-3 py-2 text-zinc-500">no backups yet</td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  )
}

/** Backups of the database containers in one project, stored on the master. */
export default function BackupsPanel({ project, session }: { project: string; session: SessionInfo }) {
  const targets = useQuery({
    queryKey: ['backup-targets'],
    queryFn: () => api<BackupTarget[]>('/backups/targets'),
    refetchInterval: 30_000,
    retry: false,
  })
  const mine = (targets.data ?? []).filter((t) => t.project === project)
  if (mine.length === 0) return null
  return (
    <div className="space-y-3">
      <h2 className="text-xs uppercase tracking-wide text-zinc-500">database backups · stored on the master</h2>
      {mine.map((t) => (
        <TargetCard key={t.service} target={t} session={session} />
      ))}
    </div>
  )
}
