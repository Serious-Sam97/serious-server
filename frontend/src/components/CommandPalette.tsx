import { useEffect, useMemo, useRef, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { useNavigate } from 'react-router'
import { anyProject, api, can } from '../api/client'
import type { Project, SessionInfo } from '../api/client'
import { navFor } from './Layout'

interface Command {
  id: string
  label: string
  hint: string
  run: () => void
}

const COMPOSE_ACTIONS = ['up', 'restart', 'pull', 'up_build', 'down'] as const

export default function CommandPalette({
  session,
  onClose,
  onLogout,
}: {
  session: SessionInfo
  onClose: () => void
  onLogout: () => void
}) {
  const navigate = useNavigate()
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const listRef = useRef<HTMLDivElement>(null)

  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: () => api<Project[]>('/projects'),
    enabled: anyProject(session, 'view'),
  })

  const commands = useMemo<Command[]>(() => {
    const go = (to: string, state?: unknown) => () => {
      navigate(to, { state })
      onClose()
    }
    const out: Command[] = navFor(session).map((n, i) => ({
      id: `nav:${n.to}`,
      label: `go ${n.label}`,
      hint: String(i + 1),
      run: go(n.to),
    }))
    for (const p of projects.data ?? []) {
      const path = `/projects/${encodeURIComponent(p.name)}`
      out.push({ id: `open:${p.name}`, label: `open ${p.name}`, hint: `${p.running}/${p.total}`, run: go(path) })
      if (can(session, 'control', p.name)) {
        for (const a of COMPOSE_ACTIONS) {
          out.push({
            id: `${a}:${p.name}`,
            label: `${a.replace('_', ' --')} ${p.name}`,
            hint: 'compose',
            run: () => {
              if (a === 'down' && !confirm(`compose down ${p.name}?`)) return
              go(path, { run: a })()
            },
          })
        }
      }
      if (session.role === 'admin') {
        out.push({
          id: `term:${p.name}`,
          label: `term ${p.name}`,
          hint: 'shell',
          run: go(`/terminal?cwd=${encodeURIComponent(p.path)}`),
        })
      }
    }
    out.push({ id: 'logout', label: 'logout', hint: '', run: onLogout })
    return out
  }, [session, projects.data, navigate, onClose, onLogout])

  const filtered = useMemo(() => {
    const terms = query.toLowerCase().split(/\s+/).filter(Boolean)
    return commands.filter((c) => terms.every((t) => c.label.toLowerCase().includes(t)))
  }, [commands, query])

  useEffect(() => setActive(0), [query])

  useEffect(() => {
    listRef.current?.children[active]?.scrollIntoView({ block: 'nearest' })
  }, [active])

  function onKey(e: React.KeyboardEvent) {
    if (e.key === 'Escape') onClose()
    else if (e.key === 'ArrowDown') {
      e.preventDefault()
      setActive((a) => Math.min(a + 1, filtered.length - 1))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setActive((a) => Math.max(a - 1, 0))
    } else if (e.key === 'Enter') {
      filtered[active]?.run()
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex justify-center bg-black/60 pt-[15vh]" onMouseDown={onClose}>
      <div
        className="flex h-fit max-h-[60vh] w-[560px] flex-col border border-zinc-700 bg-zinc-900 shadow-2xl"
        onMouseDown={(e) => e.stopPropagation()}
        role="dialog"
        aria-label="Command palette"
      >
        <label className="flex items-center gap-2 border-b border-zinc-800 px-3">
          <span className="text-accent">&gt;</span>
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onKey}
            placeholder="restart jellyfin, go system, term immich…"
            className="h-11 flex-1 bg-transparent outline-none placeholder:text-zinc-600 focus-visible:outline-none"
          />
        </label>
        <div ref={listRef} className="overflow-auto py-1" role="listbox">
          {filtered.map((c, i) => (
            <button
              key={c.id}
              type="button"
              role="option"
              aria-selected={i === active}
              onMouseEnter={() => setActive(i)}
              onClick={c.run}
              className={`flex w-full items-center gap-3 px-3 py-1.5 text-left ${
                i === active ? 'bg-zinc-800 text-zinc-50' : 'text-zinc-300'
              }`}
            >
              <span className={i === active ? 'text-accent' : 'text-transparent'}>▸</span>
              <span className="flex-1">{c.label}</span>
              <span className="text-xs text-zinc-500">{c.hint}</span>
            </button>
          ))}
          {filtered.length === 0 && <div className="px-3 py-2 text-zinc-500">no matches</div>}
        </div>
      </div>
    </div>
  )
}
