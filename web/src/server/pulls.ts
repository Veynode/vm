// Server-only: image pulls that outlive one request, so the browser can poll
// their progress: the daemon's `image.pull` stream, whose `progress` events
// are the lines kept here. One pull per reference at a time; finished pulls
// stay until the next pull of that reference.
import { stream } from '../../daemon.mjs'

const KEEP = 40

export interface PullState {
  reference: string
  state: 'pulling' | 'done' | 'failed'
  /** The latest progress lines, oldest first. */
  lines: string[]
  /** `disk chunk … done (k/n)` of a macOS image. */
  done?: number
  total?: number
  error?: string
  started_at: number
}

const pulls = new Map<string, PullState>()

export function pullState(reference: string): PullState | null {
  return pulls.get(reference) ?? null
}

export function startPull(reference: string): PullState {
  const current = pulls.get(reference)
  if (current?.state === 'pulling') return current
  const pull: PullState = { reference, state: 'pulling', lines: [], started_at: Date.now() / 1000 }
  pulls.set(reference, pull)
  const pulling = stream('image.pull', { reference })
  const line = (text: string) => {
    const trimmed = text.trim()
    if (!trimmed) return
    pull.lines.push(trimmed)
    if (pull.lines.length > KEEP) pull.lines.shift()
    const chunks = /\((\d+)\/(\d+)\)\s*$/.exec(trimmed)
    if (chunks) {
      pull.done = Number(chunks[1])
      pull.total = Number(chunks[2])
    }
    const plan = /^macOS image: (\d+) disk chunks/.exec(trimmed)
    if (plan) pull.total = Number(plan[1])
  }
  pulling.on('event', (event: { progress?: string }) => {
    if (typeof event.progress === 'string') event.progress.split('\n').forEach(line)
  })
  pulling.on('error', (error: Error) => {
    pull.state = 'failed'
    pull.error = error.message
  })
  pulling.on('end', () => {
    pull.state = 'done'
  })
  return pull
}
