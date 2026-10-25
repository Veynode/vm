// Server-only bridge to the veynodevm CLI. The CLI owns the image store,
// container records and port publishers, so the UI never reimplements them:
// every operation is `veynodevm --json ...` with an argv array (no shell).
import { execFile } from 'node:child_process'

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

export async function must<T>(args: string[], timeoutMs?: number): Promise<T> {
  const result = await veynodevm<T>(args, timeoutMs)
  if (!result.ok) throw new Error(result.error || `veynodevm ${args[0]} failed`)
  return result.data as T
}
