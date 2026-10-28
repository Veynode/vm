// Server-only bridges to the daemon: API 1 on its socket (`api`, `collect`)
// for everything the daemon serves, and the veynodevm CLI (`veynodevm --json
// ...`, an argv array, no shell) for what stays a client's: starting and
// stopping the daemon under launchd, and the docker side of the shared engine
// (`engine start|stop`, `run --shared`).
import { execFile } from 'node:child_process'
import { call, stream } from '../../daemon.mjs'

/** An API 1 method that answers once (docs/api.md). */
export function api<T = any>(method: string, args: object = {}, timeoutMs = 60_000): Promise<T> {
  return call<T>(method, args, { timeoutMs })
}

/** A streaming method to its end, with its output. */
export function collect<T = any>(method: string, args: object = {}): Promise<{ result: T; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const s = stream(method, args)
    const out: Buffer[] = []
    const err: Buffer[] = []
    s.on('stdout', (d: Buffer) => out.push(d))
    s.on('stderr', (d: Buffer) => err.push(d))
    s.on('error', reject)
    s.on('end', (result: T) =>
      resolve({ result, stdout: Buffer.concat(out).toString('utf8'), stderr: Buffer.concat(err).toString('utf8') }),
    )
  })
}

const CLI = process.env.VEYNODEVM_CLI || 'veynodevm'

export interface CliResult<T = unknown> {
  ok: boolean
  data?: T
  error?: string
  stdout: string
  stderr: string
  code: number
}

export function veynodevm<T = unknown>(args: string[], timeoutMs = 60_000): Promise<CliResult<T>> {
  return new Promise((resolve) => {
    execFile(
      CLI,
      ['--json', ...args],
      { timeout: timeoutMs, maxBuffer: 8 * 1024 * 1024, env: process.env },
      (error, stdout, stderr) => {
        const code = error && typeof (error as any).code === 'number' ? (error as any).code : error ? 1 : 0
        let data: any
        try {
          data = stdout.trim() ? JSON.parse(stdout) : undefined
        } catch {
          data = undefined
        }
        const message = data && typeof data === 'object' && 'error' in data ? String(data.error) : undefined
        resolve({
          ok: !error && !message,
          data: message ? undefined : data,
          error: message ?? (error ? (stderr.trim() || error.message) : undefined),
          stdout,
          stderr,
          code,
        })
      },
    )
  })
}

/** Turn the control daemon on if it is off: starting a container or VM needs it. */
export async function ensureDaemon(): Promise<void> {
  try {
    await api('daemon.status', {}, 10_000)
    return
  } catch {
    // Not answering: launchd starts it.
  }
  const started = await veynodevm(['daemon', 'start'], 60_000)
  if (started.code !== 0) throw new Error(started.stderr.trim().replace(/^veynodevm: /, '') || 'daemon start failed')
}
