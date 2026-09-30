import { createContext, useContext, useEffect, useState } from 'react'
import { wsUrl } from '../api/client'
import type { HistoryPoint, SystemStats } from '../api/client'

/** Server keeps 30 min at 2 s; keep the same window client-side. */
const MAX_POINTS = 900
/** No frame for this long means the numbers on screen are lying. */
const STALE_MS = 7_000

export interface StatsState {
  stats: SystemStats | null
  history: HistoryPoint[]
  connected: boolean
  stale: boolean
}

const EMPTY: StatsState = { stats: null, history: [], connected: false, stale: false }
const StatsContext = createContext<StatsState>(EMPTY)

function pointFrom(s: SystemStats): HistoryPoint {
  return [
    Math.floor(s.ts / 1000),
    s.cpu.total,
    s.mem.used,
    s.net.reduce((a, n) => a + n.rx_rate, 0),
    s.net.reduce((a, n) => a + n.tx_rate, 0),
    s.disk_io?.read_rate ?? 0,
    s.disk_io?.write_rate ?? 0,
    s.load[0],
  ]
}

/// One /ws/system socket for the whole app: the status line and the System
/// page both read from it.
export function StatsProvider({
  enabled,
  children,
}: {
  enabled: boolean
  children: React.ReactNode
}) {
  const [state, setState] = useState<StatsState>(EMPTY)

  useEffect(() => {
    if (!enabled) return
    let ws: WebSocket | null = null
    let closed = false
    let retry: ReturnType<typeof setTimeout>
    let lastFrame = 0

    function connect() {
      ws = new WebSocket(wsUrl('/ws/system'))
      ws.onopen = () => setState((s) => ({ ...s, connected: true }))
      ws.onmessage = (ev) => {
        let msg: unknown
        try {
          msg = JSON.parse(ev.data)
        } catch {
          return
        }
        const m = msg as { history?: { points: HistoryPoint[] } } & Partial<SystemStats>
        if (m.history) {
          const points = m.history.points
          setState((s) => ({ ...s, history: points.slice(-MAX_POINTS) }))
          return
        }
        if (!m.cpu) return
        const stats = m as SystemStats
        lastFrame = Date.now()
        setState((s) => {
          const p = pointFrom(stats)
          const last = s.history.at(-1)
          // The replayed history already contains the newest sample.
          const history =
            last && last[0] >= p[0] ? s.history : [...s.history, p].slice(-MAX_POINTS)
          return { stats, history, connected: true, stale: false }
        })
      }
      ws.onclose = () => {
        setState((s) => ({ ...s, connected: false }))
        if (!closed) retry = setTimeout(connect, 3000)
      }
    }
    connect()

    const watchdog = setInterval(() => {
      const stale = lastFrame > 0 && Date.now() - lastFrame > STALE_MS
      setState((s) => (s.stale === stale ? s : { ...s, stale }))
    }, 1000)

    return () => {
      closed = true
      clearTimeout(retry)
      clearInterval(watchdog)
      ws?.close()
    }
  }, [enabled])

  return <StatsContext.Provider value={state}>{children}</StatsContext.Provider>
}

export function useStats() {
  return useContext(StatsContext)
}
