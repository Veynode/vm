/** The highest rate the host encoders take (`fps N`, the fastest Mac display). */
export const MAX_FPS = 240
const SAMPLES = 40
const EVERY_MS = 5000

/**
 * The screen's refresh rate from consecutive requestAnimationFrame gaps (ms), or null
 * for too few. Callbacks land on vsync, so no gap is shorter than one refresh, while a
 * busy main thread skips some: a low percentile keeps those from halving the rate.
 */
export function refreshHz(gaps: number[]): number | null {
  const valid = gaps.filter((g) => g > 0 && Number.isFinite(g)).sort((a, b) => a - b)
  if (valid.length < SAMPLES / 2) return null
  const gap = valid[Math.floor(valid.length * 0.2)]
  return Math.max(1, Math.min(MAX_FPS, Math.round(1000 / gap)))
}

/** Whether a newly measured rate is worth telling the encoder (rounding jitter is not). */
export function changed(previous: number | null, next: number) {
  return previous === null || Math.abs(previous - next) >= 3
}

/**
 * Measures the refresh of the screen showing the page now and every few seconds while
 * the page is visible (a window dragged to another display changes it), and reports each
 * change. A hidden page runs no animation frames, so it reports nothing until it is shown.
 */
export function watchRefresh(report: (hz: number) => void): () => void {
  let stopped = false
  let last: number | null = null
  let timer: ReturnType<typeof setTimeout> | null = null
  const measure = () => {
    timer = null
    const gaps: number[] = []
    let previous: number | null = null
    const step = (now: number) => {
      if (stopped) return
      if (previous !== null) gaps.push(now - previous)
      previous = now
      if (gaps.length < SAMPLES) return void requestAnimationFrame(step)
      const hz = refreshHz(gaps)
      if (hz !== null && changed(last, hz)) report((last = hz))
      timer = setTimeout(measure, EVERY_MS)
    }
    requestAnimationFrame(step)
  }
  measure()
  return () => {
    stopped = true
    if (timer) clearTimeout(timer)
  }
}
