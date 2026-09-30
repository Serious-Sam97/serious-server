import { useEffect, useRef, useState } from 'react'
import { useSearchParams } from 'react-router'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { wsUrl } from '../api/client'

export default function TerminalPage() {
  const [params] = useSearchParams()
  const cwd = params.get('cwd')
  const containerRef = useRef<HTMLDivElement>(null)
  const [status, setStatus] = useState<'connecting' | 'open' | 'closed'>('connecting')
  const [restartKey, setRestartKey] = useState(0)

  useEffect(() => {
    const el = containerRef.current
    if (!el) return

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 14,
      fontFamily: "'JetBrains Mono Variable', 'JetBrains Mono', 'Fira Code', ui-monospace, monospace",
      theme: {
        background: '#0a0908',
        foreground: '#e8e4d8',
        cursor: '#f5a524',
        selectionBackground: '#5a4520',
      },
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(el)
    fit.fit()

    const query = new URLSearchParams()
    if (cwd) query.set('cwd', cwd)
    query.set('rows', String(term.rows))
    query.set('cols', String(term.cols))

    const ws = new WebSocket(wsUrl(`/ws/terminal?${query}`))
    ws.binaryType = 'arraybuffer'

    ws.onopen = () => {
      setStatus('open')
      term.focus()
    }
    ws.onmessage = (ev) => {
      if (typeof ev.data === 'string') {
        try {
          const msg = JSON.parse(ev.data)
          if (msg.type === 'exit') {
            term.write('\r\n\x1b[90m[session ended]\x1b[0m\r\n')
            setStatus('closed')
            ws.close()
          }
        } catch {
          /* ignore */
        }
      } else {
        term.write(new Uint8Array(ev.data))
      }
    }
    ws.onclose = () => setStatus('closed')

    const encoder = new TextEncoder()
    const dataSub = term.onData((data) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(encoder.encode(data))
    })

    let resizeTimer: ReturnType<typeof setTimeout>
    const onResize = () => {
      clearTimeout(resizeTimer)
      resizeTimer = setTimeout(() => {
        fit.fit()
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }))
        }
      }, 100)
    }
    const observer = new ResizeObserver(onResize)
    observer.observe(el)

    // Keep the connection warm through cloudflared's idle timeout.
    const ping = setInterval(() => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ type: 'ping' }))
    }, 30_000)

    return () => {
      clearInterval(ping)
      clearTimeout(resizeTimer)
      observer.disconnect()
      dataSub.dispose()
      ws.close()
      term.dispose()
    }
  }, [cwd, restartKey])

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-3 border-b border-zinc-800 px-4 py-2 text-sm">
        <span
          className={`h-2 w-2 rounded-full ${
            status === 'open'
              ? 'bg-emerald-400'
              : status === 'connecting'
                ? 'bg-amber-400'
                : 'bg-red-400'
          }`}
        />
        <span className="font-mono text-xs text-zinc-400">
          {cwd ?? 'default directory'}
        </span>
        <div className="flex-1" />
        {status === 'closed' && (
          <button
            onClick={() => setRestartKey((k) => k + 1)}
            className="rounded border border-zinc-700 px-2 py-1 text-xs hover:bg-zinc-800"
          >
            new session
          </button>
        )}
      </div>
      <div ref={containerRef} className="min-h-0 flex-1 bg-[#0a0908] p-2" />
    </div>
  )
}
