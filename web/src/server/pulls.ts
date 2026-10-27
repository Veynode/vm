// Server-only: image pulls that outlive one request, so the browser can poll
// their progress. `veynodevm pull` prints its progress on stderr only without
// --json, so this runs it plain; its last stderr line is the error when it
// fails. One pull per reference at a time; finished pulls stay until the next
// pull of that reference.
import { spawn } from 'node:child_process'

const CLI = process.env.VEYNODEVM_CLI || 'veynodevm'
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
  const child = spawn(CLI, ['pull', reference], { stdio: ['ignore', 'ignore', 'pipe'], env: process.env })
  let partial = ''
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
  child.stderr.setEncoding('utf8')
  child.stderr.on('data', (data: string) => {
    const parts = (partial + data).split('\n')
    partial = parts.pop() ?? ''
    parts.forEach(line)
  })
  child.on('error', (error) => {
    pull.state = 'failed'
    pull.error = error.message
  })
  child.on('close', (code) => {
    line(partial)
    if (pull.state !== 'pulling') return
    if (code === 0) pull.state = 'done'
    else {
      pull.state = 'failed'
      const last = pull.lines.at(-1) ?? `veynodevm pull exited with ${code}`
      pull.error = last.replace(/^veynodevm: /, '')
    }
  })
  return pull
}
