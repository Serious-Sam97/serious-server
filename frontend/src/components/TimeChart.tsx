import { useRef, useState } from 'react'

export interface Series {
  label: string
  /** null = no data (drawn as a gap, never as zero) */
  values: (number | null)[]
  /** CSS colour for the line; text never uses it. */
  color: string
  /** Fill under the line (first series only reads well). */
  area?: boolean
  /** Thinner, dashed line (e.g. peaks next to averages). */
  dashed?: boolean
}

/** A moment worth pointing at on the time axis (a restart, a backup…). */
export interface Marker {
  t: number
  label: string
  color?: string
}

/**
 * Time-series line chart that stretches to its container. One y-axis shared
 * by every series; hover shows a crosshair and each series' value.
 */
export default function TimeChart({
  series,
  times,
  max,
  format,
  height,
  markers = [],
}: {
  series: Series[]
  /** unix seconds, same length as each series' values */
  times: number[]
  /** fixed top of the scale (e.g. 100 for %); otherwise the data peak */
  max?: number
  format: (v: number) => string
  /** px; omit to fill the remaining height of a flex column */
  height?: number
  markers?: Marker[]
}) {
  const ref = useRef<HTMLDivElement>(null)
  const [hover, setHover] = useState<number | null>(null)
  const n = times.length
  const W = 1000
  const H = 100

  const peak = Math.max(
    max ?? 0,
    ...series.flatMap((s) => s.values.filter((v): v is number => v !== null && Number.isFinite(v))),
    1e-9,
  )
  const top = max ?? niceCeil(peak)
  const x = (i: number) => (n <= 1 ? W : (i / (n - 1)) * W)
  const y = (v: number) => H - (Math.min(v, top) / top) * H

  /** Pen up across gaps, so missing data never reads as a drop to zero. */
  function path(values: (number | null)[]) {
    let d = ''
    let pen = false
    values.forEach((v, i) => {
      if (v === null || !Number.isFinite(v)) {
        pen = false
        return
      }
      d += `${pen ? 'L' : 'M'}${x(i).toFixed(1)},${y(v).toFixed(1)}`
      pen = true
    })
    return d
  }
  const t0 = times[0] ?? 0
  const tN = times[n - 1] ?? t0
  const markerX = (t: number) => (tN > t0 ? ((t - t0) / (tN - t0)) * W : W)
  const fmtTime = (t: number) => {
    const d = new Date(t * 1000)
    return tN - t0 > 86_400
      ? d.toLocaleString([], { hour12: false, month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
      : d.toLocaleTimeString([], { hour12: false })
  }

  function onMove(e: React.MouseEvent) {
    const rect = ref.current?.getBoundingClientRect()
    if (!rect || n === 0) return
    const frac = (e.clientX - rect.left) / rect.width
    setHover(Math.max(0, Math.min(n - 1, Math.round(frac * (n - 1)))))
  }

  const hi = hover ?? n - 1
  const hoverTime = hover !== null && times[hover] ? fmtTime(times[hover]) : null
  const bucket = n > 1 ? (tN - t0) / (n - 1) : 0
  const hoverMarks =
    hover !== null && times[hover]
      ? markers.filter((m) => Math.abs(m.t - times[hover]) <= bucket / 2).map((m) => m.label)
      : []
  const fmtValue = (v: number | null | undefined) => (v === null || v === undefined ? '–' : format(v))

  return (
    <div className={`flex flex-col gap-1 ${height ? '' : 'min-h-24 flex-1'}`}>
      <div className="flex min-h-4 items-center gap-4 text-[11px] text-zinc-500">
        {series.length > 1 &&
          series.map((s) => (
            <span key={s.label} className="flex items-center gap-1.5">
              <span className="inline-block h-0.5 w-3" style={{ background: s.color }} />
              {s.label}
              {hi >= 0 && s.values[hi] !== undefined && (
                <span className="text-zinc-200 tabular-nums">{fmtValue(s.values[hi])}</span>
              )}
            </span>
          ))}
        {series.length === 1 && hover !== null && (
          <span className="text-zinc-200 tabular-nums">{fmtValue(series[0].values[hover])}</span>
        )}
        {hoverMarks.length > 0 && (
          <span className="truncate text-amber-300" title={hoverMarks.join(' · ')}>
            ● {hoverMarks.slice(0, 2).join(' · ')}
            {hoverMarks.length > 2 && ` +${hoverMarks.length - 2}`}
          </span>
        )}
        <span className="ml-auto shrink-0 tabular-nums">{hoverTime ?? `max ${format(top)}`}</span>
      </div>
      <div
        ref={ref}
        className={`relative cursor-crosshair border-b border-l border-zinc-800 ${height ? '' : 'flex-1'}`}
        style={height ? { height } : undefined}
        onMouseMove={onMove}
        onMouseLeave={() => setHover(null)}
      >
        <svg
          viewBox={`0 0 ${W} ${H}`}
          preserveAspectRatio="none"
          className="absolute inset-0 h-full w-full overflow-visible"
          aria-hidden="true"
        >
          {[25, 50, 75].map((g) => (
            <line
              key={g}
              x1={0}
              x2={W}
              y1={g}
              y2={g}
              stroke="var(--color-zinc-800)"
              strokeDasharray="2 6"
              vectorEffect="non-scaling-stroke"
            />
          ))}
          {series.map(
            (s) =>
              s.area &&
              n > 1 && (
                <path
                  key={`${s.label}-area`}
                  d={`${path(s.values)}L${W},${H}L0,${H}Z`}
                  fill={s.color}
                  opacity={0.12}
                />
              ),
          )}
          {series.map((s) => (
            <path
              key={s.label}
              d={path(s.values)}
              fill="none"
              stroke={s.color}
              strokeWidth={s.dashed ? 1 : 1.5}
              strokeDasharray={s.dashed ? '3 3' : undefined}
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
          ))}
          {markers
            .filter((m) => m.t >= t0 && m.t <= tN)
            .map((m, i) => (
              <line
                key={`m${i}`}
                x1={markerX(m.t)}
                x2={markerX(m.t)}
                y1={0}
                y2={H}
                stroke={m.color ?? 'var(--color-zinc-500)'}
                strokeWidth={1}
                strokeDasharray="1 3"
                vectorEffect="non-scaling-stroke"
              >
                <title>{`${fmtTime(m.t)} · ${m.label}`}</title>
              </line>
            ))}
          {hover !== null && (
            <line
              x1={x(hover)}
              x2={x(hover)}
              y1={0}
              y2={H}
              stroke="var(--color-zinc-500)"
              vectorEffect="non-scaling-stroke"
            />
          )}
        </svg>
        {hover !== null &&
          series.map((s) => (
            <span
              key={s.label}
              className="pointer-events-none absolute h-2 w-2 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-zinc-900"
              style={{
                left: `${(x(hover) / W) * 100}%`,
                top: `${(y(s.values[hover] ?? 0) / H) * 100}%`,
                display: s.values[hover] === null ? 'none' : undefined,
                background: s.color,
              }}
            />
          ))}
      </div>
    </div>
  )
}

/** Round a peak up to 1/2/5 × 10^k so the scale reads cleanly. */
function niceCeil(v: number): number {
  const exp = Math.pow(10, Math.floor(Math.log10(v)))
  for (const m of [1, 2, 5, 10]) if (v <= m * exp) return m * exp
  return 10 * exp
}
