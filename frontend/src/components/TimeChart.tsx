import { useRef, useState } from 'react'

export interface Series {
  label: string
  values: number[]
  /** CSS colour for the line; text never uses it. */
  color: string
  /** Fill under the line (first series only reads well). */
  area?: boolean
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
}: {
  series: Series[]
  /** unix seconds, same length as each series' values */
  times: number[]
  /** fixed top of the scale (e.g. 100 for %); otherwise the data peak */
  max?: number
  format: (v: number) => string
  /** px; omit to fill the remaining height of a flex column */
  height?: number
}) {
  const ref = useRef<HTMLDivElement>(null)
  const [hover, setHover] = useState<number | null>(null)
  const n = times.length
  const W = 1000
  const H = 100

  const peak = Math.max(max ?? 0, ...series.flatMap((s) => s.values), 1e-9)
  const top = max ?? niceCeil(peak)
  const x = (i: number) => (n <= 1 ? W : (i / (n - 1)) * W)
  const y = (v: number) => H - (Math.min(v, top) / top) * H

  function path(values: number[]) {
    return values.map((v, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join('')
  }

  function onMove(e: React.MouseEvent) {
    const rect = ref.current?.getBoundingClientRect()
    if (!rect || n === 0) return
    const frac = (e.clientX - rect.left) / rect.width
    setHover(Math.max(0, Math.min(n - 1, Math.round(frac * (n - 1)))))
  }

  const hi = hover ?? n - 1
  const hoverTime =
    hover !== null && times[hover]
      ? new Date(times[hover] * 1000).toLocaleTimeString([], { hour12: false })
      : null

  return (
    <div className={`flex flex-col gap-1 ${height ? '' : 'min-h-24 flex-1'}`}>
      <div className="flex min-h-4 items-center gap-4 text-[11px] text-zinc-500">
        {series.length > 1 &&
          series.map((s) => (
            <span key={s.label} className="flex items-center gap-1.5">
              <span className="inline-block h-0.5 w-3" style={{ background: s.color }} />
              {s.label}
              {hi >= 0 && s.values[hi] !== undefined && (
                <span className="text-zinc-200 tabular-nums">{format(s.values[hi])}</span>
              )}
            </span>
          ))}
        {series.length === 1 && hover !== null && (
          <span className="text-zinc-200 tabular-nums">{format(series[0].values[hover])}</span>
        )}
        <span className="ml-auto tabular-nums">{hoverTime ?? `max ${format(top)}`}</span>
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
              strokeWidth={1.5}
              strokeLinejoin="round"
              vectorEffect="non-scaling-stroke"
            />
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
