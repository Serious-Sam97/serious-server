import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Navigate } from 'react-router'
import { KeyRound, ShieldX, Trash2, UserPlus } from 'lucide-react'
import { api, ApiError } from '../api/client'
import type { AdminUser, Permissions, Project, ProjectPerms } from '../api/client'
import { useSession } from '../components/Layout'

const CAPS: (keyof ProjectPerms)[] = ['view', 'control', 'logs', 'files', 'git']

function emptyPerms(): Permissions {
  return { system: false, projects: {} }
}

function randomPassword(): string {
  const chars = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789'
  return Array.from(crypto.getRandomValues(new Uint8Array(16)))
    .map((b) => chars[b % chars.length])
    .join('')
}

function MatrixEditor({
  user,
  projects,
  onSaved,
}: {
  user: AdminUser
  projects: Project[]
  onSaved: () => void
}) {
  const [perms, setPerms] = useState<Permissions>({
    system: user.permissions.system,
    projects: { ...user.permissions.projects },
  })
  const [error, setError] = useState('')

  const save = useMutation({
    mutationFn: () =>
      api(`/admin/users/${user.id}/permissions`, {
        method: 'PUT',
        body: { permissions: perms },
      }),
    onSuccess: onSaved,
    onError: (e) => setError(e instanceof ApiError ? e.message : 'save failed'),
  })

  function toggle(project: string, cap: keyof ProjectPerms) {
    setPerms((prev) => {
      const current: ProjectPerms = prev.projects[project] ?? {
        view: false,
        control: false,
        logs: false,
        files: false,
        git: false,
      }
      const next = { ...current, [cap]: !current[cap] }
      // Anything beyond seeing the project implies seeing it.
      if (cap !== 'view' && next[cap]) next.view = true
      if (cap === 'view' && !next.view) {
        next.control = next.logs = next.files = next.git = false
      }
      return { ...prev, projects: { ...prev.projects, [project]: next } }
    })
  }

  return (
    <div className="mt-3 rounded-lg border border-zinc-800 bg-zinc-950/60 p-4">
      <label className="mb-3 flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={perms.system}
          onChange={() => setPerms((p) => ({ ...p, system: !p.system }))}
        />
        System monitor (CPU/RAM/disks/services)
      </label>
      <table className="w-full text-sm">
        <thead className="text-left text-xs uppercase tracking-wide text-zinc-500">
          <tr>
            <th className="py-1.5 pr-4">Project</th>
            {CAPS.map((c) => (
              <th key={c} className="px-2 py-1.5 text-center">
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y divide-zinc-800/60">
          {projects.map((p) => {
            const row = perms.projects[p.name]
            return (
              <tr key={p.name}>
                <td className="py-1.5 pr-4 font-mono text-xs">{p.name}</td>
                {CAPS.map((cap) => (
                  <td key={cap} className="px-2 py-1.5 text-center">
                    <input
                      type="checkbox"
                      checked={!!row?.[cap]}
                      onChange={() => toggle(p.name, cap)}
                    />
                  </td>
                ))}
              </tr>
            )
          })}
        </tbody>
      </table>
      {error && <p className="mt-2 text-sm text-red-400">{error}</p>}
      <div className="mt-3 flex justify-end">
        <button
          disabled={save.isPending}
          onClick={() => save.mutate()}
          className="rounded-md bg-accent px-3 py-1.5 text-sm font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-50"
        >
          Save permissions
        </button>
      </div>
    </div>
  )
}

export default function Users() {
  const session = useSession()
  const queryClient = useQueryClient()
  const [expanded, setExpanded] = useState<number | null>(null)
  const [newUsername, setNewUsername] = useState('')
  const [newPassword, setNewPassword] = useState(randomPassword())
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')

  const users = useQuery({
    queryKey: ['admin-users'],
    queryFn: () => api<AdminUser[]>('/admin/users'),
  })
  const projects = useQuery({
    queryKey: ['projects'],
    queryFn: () => api<Project[]>('/projects'),
  })

  const refresh = () => queryClient.invalidateQueries({ queryKey: ['admin-users'] })

  const create = useMutation({
    mutationFn: () =>
      api<{ id: number }>('/admin/users', {
        method: 'POST',
        body: {
          username: newUsername,
          temp_password: newPassword,
          permissions: emptyPerms(),
        },
      }),
    onSuccess: () => {
      setNotice(
        `User "${newUsername}" created. Temporary password: ${newPassword} — share it with them; they'll be forced to change it.`,
      )
      setError('')
      setNewUsername('')
      setNewPassword(randomPassword())
      refresh()
    },
    onError: (e) => setError(e instanceof ApiError ? e.message : 'create failed'),
  })

  const del = useMutation({
    mutationFn: (id: number) => api(`/admin/users/${id}`, { method: 'DELETE' }),
    onSuccess: refresh,
  })
  const resetTotp = useMutation({
    mutationFn: (id: number) => api(`/admin/users/${id}/reset_totp`, { method: 'POST' }),
    onSuccess: refresh,
  })
  const resetPassword = useMutation({
    mutationFn: ({ id, password }: { id: number; password: string }) =>
      api(`/admin/users/${id}/reset_password`, {
        method: 'POST',
        body: { temp_password: password },
      }),
    onSuccess: refresh,
  })

  if (session.data && session.data.role !== 'admin') {
    return <Navigate to="/" replace />
  }

  const btn =
    'flex items-center gap-1.5 rounded border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800 disabled:opacity-40'

  return (
    <div className="p-6">
      <h1 className="mb-5 text-lg font-bold">Users</h1>

      <form
        onSubmit={(e) => {
          e.preventDefault()
          create.mutate()
        }}
        className="mb-6 flex flex-wrap items-end gap-3 rounded-xl border border-zinc-800 bg-zinc-900/60 p-4"
      >
        <div>
          <label className="mb-1 block text-xs text-zinc-500">Username</label>
          <input
            value={newUsername}
            onChange={(e) => setNewUsername(e.target.value)}
            className="rounded-md border border-zinc-700 bg-zinc-950 px-3 py-1.5 text-sm outline-none focus:border-zinc-500"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs text-zinc-500">
            Temporary password
          </label>
          <input
            value={newPassword}
            onChange={(e) => setNewPassword(e.target.value)}
            className="w-56 rounded-md border border-zinc-700 bg-zinc-950 px-3 py-1.5 font-mono text-sm outline-none focus:border-zinc-500"
          />
        </div>
        <button
          disabled={create.isPending || !newUsername.trim()}
          className="flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-50"
        >
          <UserPlus size={15} /> Create user
        </button>
      </form>

      {notice && (
        <p className="mb-4 rounded-lg border border-emerald-900 bg-emerald-950/40 px-3 py-2 text-sm text-emerald-300">
          {notice}
        </p>
      )}
      {error && <p className="mb-4 text-sm text-red-400">{error}</p>}

      <div className="space-y-3">
        {users.data?.map((u) => (
          <div
            key={u.id}
            className="rounded-xl border border-zinc-800 bg-zinc-900/60 p-4"
          >
            <div className="flex flex-wrap items-center gap-3">
              <span className="font-medium">{u.username}</span>
              {u.role === 'admin' ? (
                <span className="rounded-full bg-amber-400/10 px-2 py-0.5 text-xs font-medium text-amber-400">
                  master admin
                </span>
              ) : (
                <>
                  {!u.totp_confirmed && (
                    <span className="rounded-full bg-sky-400/10 px-2 py-0.5 text-xs text-sky-400">
                      pending 2FA enrollment
                    </span>
                  )}
                  {u.must_change_password && (
                    <span className="rounded-full bg-amber-400/10 px-2 py-0.5 text-xs text-amber-400">
                      temp password
                    </span>
                  )}
                </>
              )}
              <span className="text-xs text-zinc-500">since {u.created_at}</span>
              <div className="flex-1" />
              {u.role !== 'admin' && (
                <div className="flex gap-1.5">
                  <button
                    onClick={() => setExpanded(expanded === u.id ? null : u.id)}
                    className={btn}
                  >
                    Permissions
                  </button>
                  <button
                    onClick={() => {
                      const pw = randomPassword()
                      if (
                        confirm(
                          `Reset ${u.username}'s password to a new temporary one?\n\nNew temp password: ${pw}\n\n(Their current sessions are logged out immediately.)`,
                        )
                      ) {
                        resetPassword.mutate({ id: u.id, password: pw })
                        setNotice(`Temp password for ${u.username}: ${pw}`)
                      }
                    }}
                    className={btn}
                  >
                    <KeyRound size={13} /> Reset password
                  </button>
                  <button
                    onClick={() =>
                      confirm(
                        `Force ${u.username} to re-enroll 2FA at next login?`,
                      ) && resetTotp.mutate(u.id)
                    }
                    className={btn}
                  >
                    <ShieldX size={13} /> Reset 2FA
                  </button>
                  <button
                    onClick={() =>
                      confirm(`Delete user ${u.username}? This cannot be undone.`) &&
                      del.mutate(u.id)
                    }
                    className={`${btn} text-red-400`}
                  >
                    <Trash2 size={13} /> Delete
                  </button>
                </div>
              )}
            </div>
            {expanded === u.id && u.role !== 'admin' && projects.data && (
              <MatrixEditor
                user={u}
                projects={projects.data}
                onSaved={() => {
                  setExpanded(null)
                  refresh()
                }}
              />
            )}
          </div>
        ))}
      </div>
    </div>
  )
}
