import { useQuery } from '@tanstack/react-query'
import { NavLink, Outlet } from 'react-router'
import { api } from '../api/client'
import type { Project } from '../api/client'
import { nodePath } from '../lib/node'

const GLYPH: Record<Project['status'], { g: string; cls: string }> = {
  running: { g: '●', cls: 'text-emerald-400' },
  partial: { g: '◐', cls: 'text-amber-400' },
  stopped: { g: '○', cls: 'text-red-400' },
  'not-created': { g: '·', cls: 'text-zinc-600' },
}

export function StatusBadge({ status }: { status: Project['status'] }) {
  const { g, cls } = GLYPH[status]
  return (
    <span className={`text-xs ${cls}`}>
      {g} {status}
    </span>
  )
}

export function useProjects() {
  return useQuery({
    queryKey: ['projects'],
    queryFn: () => api<Project[]>('/projects'),
    refetchInterval: 15_000,
  })
}

/// Master list on the left, the selected project (or the overview) on the right.
export default function Projects() {
  const projects = useProjects()

  return (
    <div className="flex h-full">
      <aside className="flex w-72 shrink-0 flex-col border-r border-zinc-800">
        <div className="px-4 py-3 text-[11px] tracking-widest text-zinc-500">
          PROJECTS{projects.data ? ` · ${projects.data.length}` : ''}
        </div>
        <div className="flex-1 overflow-auto">
          {projects.isLoading && <p className="px-4 text-zinc-500">scanning…</p>}
          {projects.error && <p className="px-4 text-red-400">failed to load projects</p>}
          {projects.data?.map((p) => {
            const { g, cls } = GLYPH[p.status]
            return (
              <NavLink
                key={p.path}
                to={nodePath(`/projects/${encodeURIComponent(p.name)}`)}
                className={({ isActive }) =>
                  `flex items-center gap-2.5 border-l-2 px-4 py-2 ${
                    isActive
                      ? 'border-accent bg-zinc-900 font-medium text-zinc-50'
                      : 'border-transparent hover:bg-zinc-900/60'
                  } ${p.status === 'not-created' ? 'text-zinc-500' : ''}`
                }
              >
                <span className={cls} aria-label={p.status}>
                  {g}
                </span>
                <span className="flex-1 truncate">{p.name}</span>
                {p.external && (
                  <span className="text-[10px] uppercase text-zinc-500" title="compose files outside the allowed roots">
                    ext
                  </span>
                )}
                <span
                  className={`text-xs tabular-nums ${
                    p.status === 'partial' ? 'text-amber-400' : p.status === 'stopped' ? 'text-red-400' : 'text-zinc-500'
                  }`}
                >
                  {p.total > 0 ? `${p.running}/${p.total}` : '—'}
                </span>
              </NavLink>
            )
          })}
          {projects.data?.length === 0 && (
            <p className="px-4 text-zinc-500">no compose projects found under the allowed roots</p>
          )}
        </div>
      </aside>
      <div className="min-w-0 flex-1">
        <Outlet />
      </div>
    </div>
  )
}

export function ProjectsIndex() {
  const projects = useProjects()
  const list = projects.data ?? []
  const counts = {
    running: list.filter((p) => p.status === 'running').length,
    partial: list.filter((p) => p.status === 'partial').length,
    stopped: list.filter((p) => p.status === 'stopped').length,
  }
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 text-zinc-500">
      <div className="flex gap-6 text-sm">
        <span className="text-emerald-400">● {counts.running} running</span>
        <span className="text-amber-400">◐ {counts.partial} partial</span>
        <span className="text-red-400">○ {counts.stopped} stopped</span>
      </div>
      <p>
        select a project, or press <span className="border border-zinc-700 px-1 text-zinc-300">⌘K</span> and
        type its name
      </p>
    </div>
  )
}
