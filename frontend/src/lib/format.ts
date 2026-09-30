export function fmtBytes(n: number, suffix = 'B'): string {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)}G${suffix}`
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)}M${suffix}`
  if (n >= 1 << 10) return `${(n / (1 << 10)).toFixed(0)}K${suffix}`
  return `${n.toFixed(0)}${suffix}`
}

export function fmtRate(n: number): string {
  return `${fmtBytes(n)}/s`
}

export function fmtUptime(secs: number): string {
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  return d > 0 ? `${d}d ${h}h` : `${h}h ${m}m`
}

export type Level = 'ok' | 'warn' | 'crit'

export function level(value: number, warn: number, crit: number): Level {
  return value >= crit ? 'crit' : value >= warn ? 'warn' : 'ok'
}

export const levelText: Record<Level, string> = {
  ok: 'text-zinc-100',
  warn: 'text-amber-400',
  crit: 'text-red-400',
}
