import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router'
import QRCode from 'qrcode'
import { api, ApiError } from '../api/client'

export default function Setup() {
  const navigate = useNavigate()
  const [stage, setStage] = useState<'credentials' | 'qr'>('credentials')
  const [token, setToken] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [uri, setUri] = useState('')
  const [code, setCode] = useState('')
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const canvasRef = useRef<HTMLCanvasElement>(null)

  useEffect(() => {
    api<{ needs_setup: boolean }>('/setup/status')
      .then((s) => {
        if (!s.needs_setup) navigate('/login')
      })
      .catch(() => {})
  }, [navigate])

  useEffect(() => {
    if (uri && canvasRef.current) {
      QRCode.toCanvas(canvasRef.current, uri, { width: 220, margin: 1 })
    }
  }, [uri])

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setError('')
    setBusy(true)
    try {
      if (stage === 'credentials') {
        const res = await api<{ otpauth_uri: string }>('/setup', {
          method: 'POST',
          body: { token, username, password },
        })
        setUri(res.otpauth_uri)
        setStage('qr')
      } else {
        await api('/setup/confirm', { method: 'POST', body: { code } })
        navigate('/login')
      }
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'Something went wrong.')
    } finally {
      setBusy(false)
    }
  }

  const input =
    'mb-3 w-full rounded-md border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm outline-none focus:border-zinc-500'

  return (
    <div className="flex h-screen items-center justify-center">
      <form
        onSubmit={submit}
        className="w-96 rounded-xl border border-zinc-800 bg-zinc-900/60 p-6"
      >
        <h1 className="mb-1 font-semibold">First-time setup</h1>
        {stage === 'credentials' ? (
          <>
            <p className="mb-4 text-sm text-zinc-400">
              Paste the setup token from the server journal (
              <code className="text-zinc-300">journalctl --user -u serious-server</code>
              ), then choose your admin credentials.
            </p>
            <input
              autoFocus
              placeholder="Setup token"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              className={`${input} font-mono`}
            />
            <input
              placeholder="Username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              className={input}
            />
            <input
              type="password"
              placeholder="Password (min 12 characters)"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className={input}
            />
          </>
        ) : (
          <>
            <p className="mb-4 text-sm text-zinc-400">
              Scan with your authenticator app, then enter the current code to
              finish enrollment.
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
              className={`${input} text-center font-mono text-lg tracking-[0.3em]`}
            />
          </>
        )}
        {error && <p className="mb-3 text-sm text-red-400">{error}</p>}
        <button
          disabled={busy}
          className="w-full rounded-md bg-accent py-2 text-sm font-bold text-zinc-950 hover:bg-accent-hi disabled:opacity-50"
        >
          {stage === 'credentials' ? 'Generate 2FA secret' : 'Confirm & finish'}
        </button>
      </form>
    </div>
  )
}
