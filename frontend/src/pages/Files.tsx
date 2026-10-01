import { useCallback, useEffect, useState } from 'react'
import { useSearchParams } from 'react-router'
import CodeMirror from '@uiw/react-codemirror'
import { yaml } from '@codemirror/lang-yaml'
import { json } from '@codemirror/lang-json'
import { StreamLanguage } from '@codemirror/language'
import { properties } from '@codemirror/legacy-modes/mode/properties'
import { shell } from '@codemirror/legacy-modes/mode/shell'
import { Folder, FileText as FileIcon, Link2 } from 'lucide-react'
import { api, ApiError } from '../api/client'
import type { FileEntry } from '../api/client'
import { onNodeLabel } from '../lib/node'

function langFor(name: string) {
  if (/\.ya?ml$/.test(name)) return [yaml()]
  if (/\.json$/.test(name)) return [json()]
  if (/^\.env|\.env\.|\.(ini|conf|properties|toml)$/.test(name))
    return [StreamLanguage.define(properties)]
  if (/\.(sh|zsh|bash)$/.test(name)) return [StreamLanguage.define(shell)]
  return []
}

function fmtSize(n: number): string {
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`
  if (n >= 1 << 10) return `${(n / (1 << 10)).toFixed(1)} KB`
  return `${n} B`
}

interface OpenFile {
  path: string
  name: string
  content: string
  original: string
  mtime_ms: number
}

export default function Files() {
  const [params] = useSearchParams()
  const [roots, setRoots] = useState<string[]>([])
  const [dir, setDir] = useState<string | null>(null)
  const [entries, setEntries] = useState<FileEntry[]>([])
  const [file, setFile] = useState<OpenFile | null>(null)
  const [error, setError] = useState('')
  const [confirming, setConfirming] = useState(false)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    api<string[]>('/files/roots').then((r) => {
      setRoots(r)
      const open = params.get('open')
      if (open && open.includes('/')) {
        const parent = open.substring(0, open.lastIndexOf('/'))
        setDir(parent)
        api<{ content: string; mtime_ms: number }>(
          `/files/read?path=${encodeURIComponent(open)}`,
        )
          .then((res) =>
            setFile({
              path: open,
              name: open.substring(open.lastIndexOf('/') + 1),
              content: res.content,
              original: res.content,
              mtime_ms: res.mtime_ms,
            }),
          )
          .catch(() => {})
      } else if (r.length > 0) {
        setDir(r[0])
      }
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const loadDir = useCallback(async (path: string) => {
    setError('')
    try {
      setEntries(await api<FileEntry[]>(`/files/tree?path=${encodeURIComponent(path)}`))
      setDir(path)
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to load directory')
    }
  }, [])

  useEffect(() => {
    if (dir) loadDir(dir)
  }, [dir, loadDir])

  async function openFile(entry: FileEntry) {
    if (!dir) return
    setError('')
    const path = `${dir}/${entry.name}`
    try {
      const res = await api<{ content: string; mtime_ms: number }>(
        `/files/read?path=${encodeURIComponent(path)}`,
      )
      setFile({
        path,
        name: entry.name,
        content: res.content,
        original: res.content,
        mtime_ms: res.mtime_ms,
      })
    } catch (e) {
      setError(e instanceof ApiError ? e.message : 'failed to open file')
    }
  }

  async function save() {
    if (!file) return
    setSaving(true)
    setError('')
    try {
      const res = await api<{ mtime_ms: number }>('/files/write', {
        method: 'PUT',
        body: {
          path: file.path,
          content: file.content,
          expected_mtime_ms: file.mtime_ms,
        },
      })
      setFile({ ...file, original: file.content, mtime_ms: res.mtime_ms })
      setConfirming(false)
    } catch (e) {
      setConfirming(false)
      if (e instanceof ApiError && e.status === 409) {
        setError('Conflict: the file changed on disk. Reopen it to get the latest version.')
      } else {
        setError(e instanceof ApiError ? e.message : 'save failed')
      }
    } finally {
      setSaving(false)
    }
  }

  const dirty = file !== null && file.content !== file.original
  const inRoot = roots.some((r) => dir === r)
  const parent = dir?.substring(0, dir.lastIndexOf('/')) || null

  if (roots.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-zinc-500">
        No file access granted.
      </div>
    )
  }

  return (
    <div className="flex h-full">
      <div className="flex w-80 shrink-0 flex-col border-r border-zinc-800">
        <div className="border-b border-zinc-800 p-3">
          <select
            value={roots.find((r) => dir?.startsWith(r)) ?? ''}
            onChange={(e) => setDir(e.target.value)}
            className="w-full rounded-md border border-zinc-700 bg-zinc-900 px-2 py-1.5 font-mono text-xs"
          >
            {roots.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
          <div className="mt-2 truncate font-mono text-xs text-zinc-500">{dir}</div>
        </div>
        <div className="flex-1 overflow-auto p-1">
          {!inRoot && parent && (
            <button
              onClick={() => setDir(parent)}
              className="flex w-full items-center gap-2 rounded px-2 py-1.5 text-sm text-zinc-400 hover:bg-zinc-800/60"
            >
              <Folder size={14} /> ..
            </button>
          )}
          {entries.map((entry) => (
            <button
              key={entry.name}
              onClick={() =>
                entry.type === 'dir' ? setDir(`${dir}/${entry.name}`) : openFile(entry)
              }
              className={`flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-sm hover:bg-zinc-800/60 ${
                file?.path === `${dir}/${entry.name}` ? 'bg-zinc-800' : ''
              }`}
            >
              {entry.type === 'dir' ? (
                <Folder size={14} className="shrink-0 text-sky-400" />
              ) : entry.type === 'symlink' ? (
                <Link2 size={14} className="shrink-0 text-zinc-500" />
              ) : (
                <FileIcon size={14} className="shrink-0 text-zinc-500" />
              )}
              <span className="truncate">{entry.name}</span>
              {entry.type !== 'dir' && (
                <span className="ml-auto shrink-0 text-xs text-zinc-600">
                  {fmtSize(entry.size)}
                </span>
              )}
            </button>
          ))}
        </div>
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        {file ? (
          <>
            <div className="flex items-center gap-3 border-b border-zinc-800 px-4 py-2">
              <span className="truncate font-mono text-sm">
                {file.path}
                {dirty && <span className="ml-1 text-amber-400">●</span>}
              </span>
              <div className="flex-1" />
              <button
                disabled={!dirty || saving}
                onClick={() => setConfirming(true)}
                className="rounded-md bg-accent px-3 py-1.5 text-sm font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-40"
              >
                Save
              </button>
            </div>
            <div className="min-h-0 flex-1 overflow-auto">
              <CodeMirror
                value={file.content}
                onChange={(value) => setFile((f) => (f ? { ...f, content: value } : f))}
                extensions={langFor(file.name)}
                theme="dark"
                height="100%"
                style={{ height: '100%' }}
              />
            </div>
          </>
        ) : (
          <div className="flex flex-1 items-center justify-center text-zinc-600">
            select a file to edit
          </div>
        )}
        {error && (
          <div className="border-t border-zinc-800 bg-red-950/40 px-4 py-2 text-sm text-red-400">
            {error}
          </div>
        )}
      </div>

      {confirming && file && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
          <div className="w-96 rounded-xl border border-zinc-700 bg-zinc-900 p-5">
            <h2 className="font-semibold">Save changes{onNodeLabel()}?</h2>
            <p className="mt-2 break-all font-mono text-sm text-zinc-400">{file.path}</p>
            <p className="mt-2 text-sm text-zinc-400">
              This overwrites the file on the server immediately.
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setConfirming(false)}
                className="rounded-md border border-zinc-700 px-3 py-1.5 text-sm hover:bg-zinc-800"
              >
                Cancel
              </button>
              <button
                disabled={saving}
                onClick={save}
                className="rounded-md bg-accent px-3 py-1.5 text-sm font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-50"
              >
                {saving ? 'Saving…' : 'Save'}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
