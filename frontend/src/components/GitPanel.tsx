import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate } from 'react-router'
import {
  ArrowDownToLine,
  ArrowUpFromLine,
  Check,
  GitBranch,
  GitMerge,
  Package,
  PackageOpen,
  RefreshCw,
  X,
} from 'lucide-react'
import { api, ApiError } from '../api/client'
import type { GitOutput, GitStatus } from '../api/client'

export default function GitPanel({
  project,
  projectPath,
  canFiles,
}: {
  project: string
  projectPath: string
  canFiles: boolean
}) {
  const navigate = useNavigate()
  const queryClient = useQueryClient()
  const [output, setOutput] = useState<string>('')
  const [showOutput, setShowOutput] = useState(false)

  const status = useQuery({
    queryKey: ['git', project],
    queryFn: () => api<GitStatus>(`/git/${encodeURIComponent(project)}/status`),
    refetchInterval: 30_000,
  })

  const refresh = () => queryClient.invalidateQueries({ queryKey: ['git', project] })

  function handleResult(res: GitOutput) {
    setOutput(res.output.trim())
    setShowOutput(res.output.trim().length > 0)
    refresh()
  }
  function handleError(e: unknown) {
    setOutput(e instanceof ApiError ? e.message : 'command failed')
    setShowOutput(true)
    refresh()
  }

  const act = useMutation({
    mutationFn: (action: string) =>
      api<GitOutput>(`/git/${encodeURIComponent(project)}/${action}`, {
        method: 'POST',
      }),
    onSuccess: handleResult,
    onError: handleError,
  })

  const resolve = useMutation({
    mutationFn: ({ path, side }: { path: string; side: 'ours' | 'theirs' }) =>
      api<GitOutput>(`/git/${encodeURIComponent(project)}/resolve`, {
        method: 'POST',
        body: { path, side },
      }),
    onSuccess: handleResult,
    onError: handleError,
  })

  const markResolved = useMutation({
    mutationFn: (path: string) =>
      api<GitOutput>(`/git/${encodeURIComponent(project)}/mark_resolved`, {
        method: 'POST',
        body: { path },
      }),
    onSuccess: handleResult,
    onError: handleError,
  })

  const s = status.data
  if (!s) return null
  if (!s.is_repo) return null

  const busy = act.isPending || resolve.isPending || markResolved.isPending
  const conflicts = s.files?.filter((f) => f.conflicted) ?? []
  const cleanFiles = s.files?.filter((f) => !f.conflicted) ?? []

  const btn =
    'flex items-center gap-1.5 rounded-md border border-zinc-700 px-2.5 py-1.5 text-xs hover:bg-zinc-800 disabled:opacity-40'

  return (
    <div className="mb-4 rounded-xl border border-zinc-800 bg-zinc-900/60 p-4">
      <div className="flex flex-wrap items-center gap-3">
        <span className="flex items-center gap-1.5 text-sm font-medium">
          <GitBranch size={15} className="text-zinc-400" />
          {s.branch}
        </span>
        {s.ahead != null && s.behind != null && (
          <span className="text-xs text-zinc-400">
            {s.ahead > 0 && <span className="text-amber-400">↑{s.ahead} </span>}
            {s.behind > 0 && <span className="text-sky-400">↓{s.behind}</span>}
            {s.ahead === 0 && s.behind === 0 && 'up to date'}
          </span>
        )}
        {(s.stash_count ?? 0) > 0 && (
          <span className="text-xs text-zinc-500">{s.stash_count} stashed</span>
        )}
        {s.in_merge && (
          <span className="rounded-full bg-amber-400/10 px-2 py-0.5 text-xs font-medium text-amber-400">
            merge in progress
          </span>
        )}
        <div className="flex-1" />
        <div className="flex flex-wrap gap-1.5">
          <button disabled={busy} onClick={() => act.mutate('fetch')} className={btn}>
            <RefreshCw size={13} /> Fetch
          </button>
          <button disabled={busy} onClick={() => act.mutate('pull')} className={btn}>
            <ArrowDownToLine size={13} /> Pull
          </button>
          <button disabled={busy} onClick={() => act.mutate('push')} className={btn}>
            <ArrowUpFromLine size={13} /> Push
          </button>
          <button disabled={busy} onClick={() => act.mutate('stash')} className={btn}>
            <Package size={13} /> Stash
          </button>
          <button
            disabled={busy || (s.stash_count ?? 0) === 0}
            onClick={() => act.mutate('stash_pop')}
            className={btn}
          >
            <PackageOpen size={13} /> Pop
          </button>
        </div>
      </div>

      {s.in_merge && (
        <div className="mt-3 flex items-center gap-2 rounded-lg border border-amber-900/60 bg-amber-950/30 p-3">
          <span className="flex-1 text-sm text-amber-200">
            {conflicts.length > 0
              ? `${conflicts.length} conflicted file${conflicts.length > 1 ? 's' : ''} to resolve`
              : 'All conflicts resolved — complete the merge.'}
          </span>
          <button
            disabled={busy || conflicts.length > 0}
            onClick={() => act.mutate('merge_continue')}
            className={`${btn} border-emerald-800 text-emerald-300`}
          >
            <GitMerge size={13} /> Complete merge
          </button>
          <button
            disabled={busy}
            onClick={() =>
              confirm('Abort the merge and return to the pre-pull state?') &&
              act.mutate('merge_abort')
            }
            className={`${btn} border-red-900 text-red-300`}
          >
            <X size={13} /> Abort merge
          </button>
        </div>
      )}

      {conflicts.length > 0 && (
        <div className="mt-3 space-y-1.5">
          {conflicts.map((f) => (
            <div
              key={f.path}
              className="flex flex-wrap items-center gap-2 rounded-md border border-red-900/60 bg-red-950/20 px-3 py-1.5"
            >
              <span className="flex-1 font-mono text-xs text-red-200">{f.path}</span>
              <button
                disabled={busy}
                onClick={() => resolve.mutate({ path: f.path, side: 'ours' })}
                className={btn}
                title="Keep this machine's version"
              >
                Keep ours
              </button>
              <button
                disabled={busy}
                onClick={() => resolve.mutate({ path: f.path, side: 'theirs' })}
                className={btn}
                title="Take the incoming version"
              >
                Take theirs
              </button>
              {canFiles && (
                <button
                  onClick={() =>
                    navigate(`/files?open=${encodeURIComponent(`${projectPath}/${f.path}`)}`)
                  }
                  className={btn}
                  title="Edit the conflict markers by hand in the file editor"
                >
                  Edit
                </button>
              )}
              <button
                disabled={busy}
                onClick={() => markResolved.mutate(f.path)}
                className={btn}
                title="Mark as resolved after editing manually"
              >
                <Check size={13} /> Resolved
              </button>
            </div>
          ))}
        </div>
      )}

      {cleanFiles.length > 0 && (
        <div className="mt-3 text-xs text-zinc-400">
          <span className="text-zinc-500">changes: </span>
          {cleanFiles.slice(0, 8).map((f) => (
            <span key={f.path} className="mr-3 font-mono">
              <span className="text-amber-400">{f.status}</span> {f.path}
            </span>
          ))}
          {cleanFiles.length > 8 && <span>+{cleanFiles.length - 8} more</span>}
        </div>
      )}

      {showOutput && output && (
        <div className="mt-3">
          <button
            onClick={() => setShowOutput(false)}
            className="mb-1 text-xs text-zinc-500 hover:text-zinc-300"
          >
            hide output ✕
          </button>
          <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded-md bg-zinc-950 p-2.5 font-mono text-xs text-zinc-300">
            {output}
          </pre>
        </div>
      )}

      {s.log && s.log.length > 0 && (
        <details className="mt-3">
          <summary className="cursor-pointer text-xs text-zinc-500 hover:text-zinc-300">
            recent commits
          </summary>
          <div className="mt-2 space-y-1">
            {s.log.map((c) => (
              <div key={c.hash} className="flex gap-2 font-mono text-xs">
                <span className="text-amber-400">{c.hash}</span>
                <span className="flex-1 truncate text-zinc-300">{c.subject}</span>
                <span className="shrink-0 text-zinc-500">
                  {c.author} · {c.when}
                </span>
              </div>
            ))}
          </div>
        </details>
      )}
    </div>
  )
}
