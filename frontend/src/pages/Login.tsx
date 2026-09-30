import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router'
import QRCode from 'qrcode'
import { api, ApiError } from '../api/client'

type Stage = 'password' | 'totp' | 'change_password' | 'enroll'

export default function Login() {
  const navigate = useNavigate()
  const [stage, setStage] = useState<Stage>('password')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [code, setCode] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [uri, setUri] = useState('')
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    api<{ needs_setup: boolean }>('/setup/status')
      .then((s) => {
        if (s.needs_setup) navigate('/setup')
      })
      .catch(() => {})
  }, [navigate])

  useEffect(() => {
    if (stage === 'enroll' && uri && canvasRef.current) {
      QRCode.toCanvas(canvasRef.current, uri, { width: 220, margin: 1 })
    }
  }, [stage, uri])

  function enterStage(next: string | undefined) {
    setError('')
    setCode('')
    if (next === 'totp') setStage('totp')
    else if (next === 'change_password') setStage('change_password')
    else if (next === 'totp_enroll') beginEnroll()
    else window.location.href = '/'
  }

  async function beginEnroll() {
    try {
      const res = await api<{ otpauth_uri: string }>('/auth/enroll/begin', {
        method: 'POST',
        body: {},
      })
      setUri(res.otpauth_uri)
      setStage('enroll')
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'enrollment failed')
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setError('')
    setBusy(true)
    try {
      if (stage === 'password') {
        const res = await api<{ next?: string }>('/auth/login', {
          method: 'POST',
          body: { username, password },
        })
        enterStage(res?.next)
      } else if (stage === 'totp') {
        const res = await api<{ next?: string } | undefined>('/auth/totp', {
          method: 'POST',
          body: { code },
        })
        enterStage(res?.next)
      } else if (stage === 'change_password') {
        if (newPassword !== confirmPassword) {
          setError('Passwords do not match.')
          return
        }
        const res = await api<{ next?: string } | undefined>('/auth/change_password', {
          method: 'POST',
          body: { new_password: newPassword },
        })
        enterStage(res?.next)
      } else {
        await api('/auth/enroll/confirm', { method: 'POST', body: { code } })
        window.location.href = '/'
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 429) {
        setError('Too many attempts — wait a minute and try again.')
      } else if (err instanceof ApiError && err.status === 400) {
        setError(err.message)
      } else {
        setError(
          stage === 'password' ? 'Invalid credentials.' : 'Invalid code.',
        )
      }
    } finally {
      setBusy(false)
    }
  }

  const input =
    'mb-3 w-full rounded-md border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm outline-none focus:border-zinc-500'
  const codeInput = `${input} text-center font-mono text-lg tracking-[0.3em]`

  return (
    <div className="flex h-screen items-center justify-center">
      <form
        onSubmit={submit}
        className="w-96 rounded-xl border border-zinc-800 bg-zinc-900/60 p-6"
      >
        <div className="mb-5 text-center">
          <div className="text-2xl">🖥️</div>
          <h1 className="mt-1 font-semibold">serious-server</h1>
        </div>

        {stage === 'password' && (
          <>
            <input
              autoFocus
              placeholder="Username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              className={input}
            />
            <input
              type="password"
              placeholder="Password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className={input}
            />
          </>
        )}

        {stage === 'totp' && (
          <input
            autoFocus
            inputMode="numeric"
            placeholder="6-digit code"
            value={code}
            onChange={(e) => setCode(e.target.value)}
            className={codeInput}
          />
        )}

        {stage === 'change_password' && (
          <>
            <p className="mb-4 text-sm text-zinc-400">
              You're using a temporary password — choose your own to continue.
            </p>
            <input
              autoFocus
              type="password"
              placeholder="New password (min 12 characters)"
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
              className={input}
            />
            <input
              type="password"
              placeholder="Confirm new password"
              value={confirmPassword}
              onChange={(e) => setConfirmPassword(e.target.value)}
              className={input}
            />
          </>
        )}

        {stage === 'enroll' && (
          <>
            <p className="mb-4 text-sm text-zinc-400">
              Scan with your authenticator app, then enter the current code to
              finish setting up 2FA.
            </p>
            <div className="mb-4 flex justify-center rounded-lg bg-white p-3">
              <canvas ref={canvasRef} />
            </div>
            <input
              autoFocus
              inputMode="numeric"
              placeholder="6-digit code"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              className={codeInput}
            />
          </>
        )}

        {error && <p className="mb-3 text-sm text-red-400">{error}</p>}
        <button
          disabled={busy}
          className="w-full rounded-md bg-accent py-2 text-sm font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-50"
        >
          {stage === 'password'
            ? 'Continue'
            : stage === 'change_password'
              ? 'Set password'
              : 'Verify'}
        </button>
      </form>
    </div>
  )
}
