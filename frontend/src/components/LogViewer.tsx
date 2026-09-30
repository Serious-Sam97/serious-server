import { useEffect, useRef, useState } from 'react'
import { wsUrl } from '../api/client'

export default function LogViewer({ containerId }: { containerId: string }) {
  const [lines, setLines] = useState<string[]>([])
  const [paused, setPaused] = useState(false)
  const pausedRef = useRef(false)
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    setLines([])
    const ws = new WebSocket(wsUrl(`/ws/containers/${containerId}/logs?tail=200`))
    ws.onmessage = (ev) => {
      setLines((prev) => {
        const next = [...prev, ...String(ev.data).split('\n').filter(Boolean)]
        return next.length > 2000 ? next.slice(-2000) : next
      })
    }
    return () => ws.close()
  }, [containerId])

  useEffect(() => {
    pausedRef.current = paused
  }, [paused])

  useEffect(() => {
    if (!pausedRef.current) bottomRef.current?.scrollIntoView({ behavior: 'instant' })
  }, [lines])

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center justify-end border-b border-zinc-800 px-3 py-1.5">
        <button
          onClick={() => setPaused((p) => !p)}
          className="rounded px-2 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800"
        >
          {paused ? '▶ resume scroll' : '⏸ pause scroll'}
        </button>
      </div>
      <pre className="flex-1 overflow-auto whitespace-pre-wrap break-all p-3 font-mono text-xs leading-relaxed text-zinc-300">
        {lines.join('\n')}
        <div ref={bottomRef} />
      </pre>
    </div>
  )
}
