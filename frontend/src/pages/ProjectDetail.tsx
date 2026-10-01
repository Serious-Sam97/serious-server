import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useLocation, useNavigate, useParams } from 'react-router'
import { SquareTerminal } from 'lucide-react'
import { api, can } from '../api/client'
import type { ContainerInfo, Job, Project } from '../api/client'
import { StatusBadge } from './Projects'
import LogViewer from '../components/LogViewer'
import BackupsPanel from '../components/BackupsPanel'
import GitPanel from '../components/GitPanel'
import { useSession, useCurrentNode } from '../components/Layout'
import { confirmOnNode, currentNode, HOME, nodeAllows, nodePath } from '../lib/node'

function useJobPolling(jobId: number | null, onDone: () => void) {
  const [job, setJob] = useState<Job | null>(null)
  useEffect(() => {
    if (jobId === null) return
    setJob(null)
    const timer = setInterval(async () => {
      try {
        const j = await api<Job>(`/jobs/${jobId}`)
        setJob(j)
        if (j.status !== 'running') {
          clearInterval(timer)
          onDone()
        }
      } catch {
        clearInterval(timer)
      }
    }, 1500)
    return () => clearInterval(timer)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [jobId])
  return job
}

export default function ProjectDetail() {
  const { name = '' } = useParams()
  const navigate = useNavigate()
  const fleetNode = useCurrentNode()
  const location = useLocation()
  const session = useSession()
  const s = session.data
  const queryClient = useQueryClient()
  const [logContainer, setLogContainer] = useState<ContainerInfo | null>(null)
  const [jobId, setJobId] = useState<number | null>(null)

  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: () => api<Project[]>('/projects'),
  })
  const project = projects.data?.find((p) => p.name === name)

  const containers = useQuery({
    queryKey: ['containers', name],
    queryFn: () => api<ContainerInfo[]>(`/projects/${encodeURIComponent(name)}/containers`),
    refetchInterval: 5_000,
  })

  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ['containers', name] })
    queryClient.invalidateQueries({ queryKey: ['projects'] })
  }

  const job = useJobPolling(jobId, refresh)

  const composeAction = useMutation({
    mutationFn: (action: string) =>
      api<{ job_id: number }>(`/compose/${encodeURIComponent(name)}/${action}`, {
        method: 'POST',
      }),
    onSuccess: (res) => setJobId(res.job_id),
  })

  const containerAction = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) =>
      api(`/containers/${id}/${action}`, { method: 'POST' }),
    onSuccess: refresh,
  })

  // Compose action requested from the command palette.
  const pendingRun = (location.state as { run?: string } | null)?.run
  useEffect(() => {
    if (!pendingRun || !can(s, 'control', name)) return
    navigate(location.pathname, { replace: true, state: null })
    composeAction.mutate(pendingRun)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pendingRun, s, name])

  const btn =
    'h-7 border border-zinc-700 px-3 hover:bg-zinc-800 disabled:opacity-40'

  return (
    <div className="flex h-full flex-col p-5">
      <div className="mb-1 flex items-center gap-3">
        <h1 className="text-lg font-bold">{name}</h1>
        {project && <StatusBadge status={project.status} />}
        <div className="flex-1" />
        {project && s?.role === 'admin' && (currentNode() === HOME || nodeAllows(fleetNode, 'terminal')) && (
          <button
            onClick={() => navigate(nodePath(`/terminal?cwd=${encodeURIComponent(project.path)}`))}
            className={`${btn} flex items-center gap-1.5`}
          >
            <SquareTerminal size={14} /> terminal here
          </button>
        )}
      </div>

      {project && (
        <div className="mb-4 truncate text-xs text-zinc-500">{project.path}</div>
      )}

      <div className="mb-4 flex flex-wrap gap-2">
        {can(s, 'control', name) &&
          [
            { action: 'up', label: 'up' },
            { action: 'up_build', label: 'up --build' },
            { action: 'build', label: 'build' },
            { action: 'down', label: 'down' },
            { action: 'restart', label: 'restart' },
            { action: 'pull', label: 'pull' },
          ].map(({ action, label }) => (
            <button
              key={action}
              disabled={composeAction.isPending || job?.status === 'running'}
              onClick={() =>
                confirmOnNode(`compose ${label} ${name}`, action === 'down') &&
                composeAction.mutate(action)
              }
              className={
                action === 'up'
                  ? 'h-7 bg-accent px-3 font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-40'
                  : action === 'down'
                    ? `${btn} border-red-400/40 text-red-400`
                    : btn
              }
            >
              {label}
            </button>
          ))}
      </div>

      {project && can(s, 'git', name) && (
        <GitPanel
          project={name}
          projectPath={project.path}
          canFiles={can(s, 'files', name)}
        />
      )}

      {job && (
        <div className="mb-4 border border-zinc-800 bg-zinc-900 p-3">
          <div className="mb-1 text-sm">
            job <span className="font-mono">{job.action}</span> —{' '}
            <span
              className={
                job.status === 'done'
                  ? 'text-emerald-400'
                  : job.status === 'failed'
                    ? 'text-red-400'
                    : 'text-amber-400'
              }
            >
              {job.status}
            </span>
          </div>
          {job.output && (
            <pre className="max-h-48 overflow-auto whitespace-pre-wrap font-mono text-xs text-zinc-400">
              {job.output}
            </pre>
          )}
        </div>
      )}

      <div className="overflow-hidden border border-zinc-800">
        <table className="w-full">
          <thead className="bg-zinc-900 text-left text-[11px] uppercase tracking-widest text-zinc-500">
            <tr>
              <th className="px-4 py-2.5">Service</th>
              <th className="px-4 py-2.5">Container</th>
              <th className="px-4 py-2.5">Image</th>
              <th className="px-4 py-2.5">Status</th>
              <th className="px-4 py-2.5 text-right">Actions</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-dashed divide-zinc-800">
            {containers.data?.map((c) => (
              <tr key={c.id}>
                <td className="px-4 py-2 font-medium">{c.service || '—'}</td>
                <td className="px-4 py-2 font-mono text-xs">{c.name}</td>
                <td className="max-w-48 truncate px-4 py-2 font-mono text-xs text-zinc-400">
                  {c.image}
                </td>
                <td className="px-4 py-2">
                  <span
                    className={
                      c.state === 'running' ? 'text-emerald-400' : 'text-zinc-400'
                    }
                  >
                    {c.status}
                  </span>
                </td>
                <td className="px-4 py-2 text-right">
                  <div className="flex justify-end gap-1.5">
                    {can(s, 'control', name) &&
                    (c.state === 'running'
                      ? ['stop', 'restart']
                      : ['start']
                    ).map((action) => (
                      <button
                        key={action}
                        disabled={containerAction.isPending}
                        onClick={() =>
                          confirmOnNode(`${action} ${c.service || c.name}`) &&
                          containerAction.mutate({ id: c.id, action })
                        }
                        className="rounded border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:opacity-40"
                      >
                        {action}
                      </button>
                    ))}
                    {can(s, 'logs', name) && (
                    <button
                      onClick={() =>
                        setLogContainer(logContainer?.id === c.id ? null : c)
                      }
                      className={`rounded border px-2 py-1 text-xs hover:bg-zinc-800 ${
                        logContainer?.id === c.id
                          ? 'border-zinc-500 bg-zinc-800'
                          : 'border-zinc-700'
                      }`}
                    >
                      logs
                    </button>
                    )}
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {s && (
        <div className="mt-4">
          <BackupsPanel project={name} session={s} />
        </div>
      )}

      {logContainer && (
        <div className="mt-4 flex min-h-72 flex-1 flex-col overflow-hidden border border-zinc-800 bg-black/30">
          <div className="border-b border-zinc-800 px-3 py-2 text-xs text-zinc-300">
            logs: {logContainer.name}
          </div>
          <div className="min-h-0 flex-1">
            <LogViewer containerId={logContainer.id} />
          </div>
        </div>
      )}
    </div>
  )
}
