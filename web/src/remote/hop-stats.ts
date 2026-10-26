// Per-hop latency (plan §4 "逐跳延迟"): samples per hop, summarised as p50
// and p95 every 10 s, exportable as JSON. Cross-machine hops come from
// NTP-style ping estimates and are for display only.

export const HOPS = [
  { id: 'encode', label: 'Capture → encoded' },
  { id: 'hostQueue', label: 'Host queue' },
  { id: 'tunnel', label: 'Host ↔ gateway (RTT/2)' },
  { id: 'gateway', label: 'Gateway dwell' },
  { id: 'edge', label: 'Gateway ↔ browser (RTT/2)' },
  { id: 'decode', label: 'Received → decoded' },
  { id: 'paint', label: 'Decoded → painted' },
  { id: 'keyToPaint', label: 'Key → paint (end to end)' },
] as const
export type HopId = (typeof HOPS)[number]['id']

export const WINDOW_MS = 10_000

export interface HopSummary {
  n: number
  p50: number | null
  p95: number | null
}

export type HopReport = { at: number; windowMs: number; hops: Record<HopId, HopSummary> }

/** Nearest-rank percentile of `values` (unsorted), or null when empty. */
export function percentile(values: readonly number[], p: number): number | null {
  if (!values.length) return null
  const sorted = [...values].sort((a, b) => a - b)
  const rank = Math.ceil((p / 100) * sorted.length)
  return sorted[Math.min(sorted.length, Math.max(1, rank)) - 1]
}

export class HopStats {
  private samples = new Map<HopId, number[]>()
  private windowStart: number
  private readonly windowMs: number
  /** The last completed window's report. */
  last: HopReport | undefined

  constructor(now: number, windowMs = WINDOW_MS) {
    this.windowStart = now
    this.windowMs = windowMs
  }

  record(hop: HopId, ms: number) {
    if (!Number.isFinite(ms) || ms < 0) return
    let list = this.samples.get(hop)
    if (!list) this.samples.set(hop, (list = []))
    if (list.length < 10_000) list.push(ms)
  }

  /** Closes the window once it has run `windowMs`; returns the new report then. */
  tick(now: number): HopReport | undefined {
    if (now - this.windowStart < this.windowMs) return undefined
    this.last = this.summary(now)
    this.samples.clear()
    this.windowStart = now
    return this.last
  }

  summary(now: number): HopReport {
    const hops = {} as Record<HopId, HopSummary>
    for (const { id } of HOPS) {
      const values = this.samples.get(id) ?? []
      hops[id] = { n: values.length, p50: percentile(values, 50), p95: percentile(values, 95) }
    }
    return { at: now, windowMs: now - this.windowStart, hops }
  }
}

/**
 * Key → paint: inputs carry a seq, video_v2 frames the highest input seq the
 * host had applied. A painted frame whose `input_ack_seq` covers a sent input
 * completes that input's sample.
 */
export class InputToPaint {
  private sent: { seq: number; at: number }[] = []

  input(seq: number, at: number) {
    this.sent.push({ seq, at })
    if (this.sent.length > 256) this.sent.shift()
  }

  /** Samples (ms) completed by a frame painted at `paintedAt` acknowledging `ackSeq`. */
  painted(ackSeq: number, paintedAt: number): number[] {
    const done: number[] = []
    while (this.sent.length && this.sent[0].seq <= ackSeq) done.push(paintedAt - this.sent.shift()!.at)
    return done
  }
}
