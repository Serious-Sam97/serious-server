import { useQuery } from '@tanstack/react-query'
import { api } from '../api/client'
import type { AuditEntry } from '../api/client'

export default function Audit() {
  const audit = useQuery({
    queryKey: ['audit'],
    queryFn: () => api<AuditEntry[]>('/audit?limit=200'),
    refetchInterval: 15_000,
  })

  return (
    <div className="p-6">
      <h1 className="mb-5 text-lg font-bold">Audit log</h1>
      <div className="overflow-hidden rounded-xl border border-zinc-800">
        <table className="w-full text-sm">
          <thead className="bg-zinc-900 text-left text-xs uppercase tracking-wide text-zinc-500">
            <tr>
              <th className="px-4 py-2.5">Time (UTC)</th>
              <th className="px-4 py-2.5">IP</th>
              <th className="px-4 py-2.5">Actor</th>
              <th className="px-4 py-2.5">Action</th>
              <th className="px-4 py-2.5">Detail</th>
              <th className="px-4 py-2.5">OK</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-zinc-800/70">
            {audit.data?.map((e) => (
              <tr key={e.id} className="bg-zinc-900/40">
                <td className="whitespace-nowrap px-4 py-2 font-mono text-xs text-zinc-400">
                  {e.ts}
                </td>
                <td className="px-4 py-2 font-mono text-xs">{e.ip}</td>
                <td className="px-4 py-2 font-mono text-xs">{e.actor}</td>
                <td className="px-4 py-2 font-mono text-xs">{e.action}</td>
                <td className="max-w-md truncate px-4 py-2 font-mono text-xs text-zinc-400">
                  {e.detail}
                </td>
                <td className="px-4 py-2">
                  <span className={e.ok ? 'text-emerald-400' : 'text-red-400'}>
                    {e.ok ? '✓' : '✗'}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}
