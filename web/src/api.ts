import { createServerFn } from '@tanstack/react-start'

export interface Port {
  host_address: string
  host_port: number
  guest_port: number
  protocol: string
}
export interface ContainerRow {
  id: string
  name: string
  image: string
  command: string[]
  created_at: number
  state: string
  status: string
  ports: Port[]
  backend: string
  gpus: string | null
  desktop: boolean
  vm_id: string
}
export interface ImageRow {
  reference: string
  config_digest: string
  size: number
  created_at: number
  config: { entrypoint: string[]; cmd: string[]; exposed_ports: string[]; env: string[] }
}
export interface RunInput {
  image: string
  command: string[]
  name?: string
  ports: string[]
  volumes: string[]
  env: string[]
  cpus?: number
  memory?: string
  gpus?: string
  desktop?: boolean
  shared?: boolean
}

const NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/
const IMAGE = /^[a-z0-9][a-z0-9._/:@-]{0,255}$/

function id(input: unknown): string {
  if (typeof input !== 'string' || !NAME.test(input)) throw new Error('invalid container id or name')
  return input
}

function image(input: unknown): string {
  if (typeof input !== 'string' || !IMAGE.test(input)) throw new Error('invalid image reference')
  return input
}

function list(input: unknown, max: number, label: string): string[] {
  if (input === undefined) return []
  if (!Array.isArray(input) || input.length > max || input.some((v) => typeof v !== 'string' || v.length > 4096 || v.includes('\0')))
    throw new Error(`invalid ${label}`)
  return input as string[]
}

export const listContainers = createServerFn({ method: 'GET' }).handler(async () => {
  const { must } = await import('./server/veynodevm')
  return must<ContainerRow[]>(['ps', '-a'])
})

export const listImages = createServerFn({ method: 'GET' }).handler(async () => {
  const { must } = await import('./server/veynodevm')
  return must<ImageRow[]>(['images'])
})

export const inspectContainer = createServerFn({ method: 'GET' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    return must<any>(['inspect', data])
  })

export const containerLogs = createServerFn({ method: 'GET' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['logs', data])
    // logs prints plain text even with --json.
    const text = result.stdout
    return text.length > 256 * 1024 ? text.slice(-256 * 1024) : text
  })

export const containerAction = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { action?: unknown; id?: unknown }
    if (!['start', 'stop', 'rm'].includes(value?.action as string)) throw new Error('invalid action')
    return { action: value.action as 'start' | 'stop' | 'rm', id: id(value.id) }
  })
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    const args = data.action === 'rm' ? ['rm', '-f', data.id] : [data.action, data.id]
    // start/stop/rm print names, not JSON; failures carry a JSON error.
    await must(args, 120_000)
    return { ok: true }
  })

export const execInContainer = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; argv?: unknown }
    const argv = list(value?.argv, 128, 'argv')
    if (argv.length === 0) throw new Error('command required')
    return { id: id(value.id), argv }
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm<any>(['exec', data.id, ...data.argv], 40_000)
    if (!result.data) throw new Error(result.error || 'exec failed')
    return result.data as { exit_code: number; stdout: string; stderr: string; timed_out: boolean }
  })

export const pullImage = createServerFn({ method: 'POST' })
  .validator((input: unknown) => image(input))
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    return must<ImageRow>(['pull', data], 30 * 60_000)
  })

export const removeImage = createServerFn({ method: 'POST' })
  .validator((input: unknown) => image(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['rmi', data])
    if (result.code !== 0) throw new Error(result.stderr.trim() || 'rmi failed')
    return { ok: true }
  })

export const runContainer = createServerFn({ method: 'POST' })
  .validator((input: unknown): RunInput => {
    const v = (input ?? {}) as Record<string, unknown>
    const out: RunInput = {
      image: image(v.image),
      command: list(v.command, 128, 'command'),
      ports: list(v.ports, 32, 'ports'),
      volumes: list(v.volumes, 8, 'volumes'),
      env: list(v.env, 64, 'env'),
    }
    if (v.name) out.name = id(v.name)
    if (v.cpus !== undefined) {
      const n = Number(v.cpus)
      if (!Number.isInteger(n) || n < 1 || n > 32) throw new Error('invalid cpus')
      out.cpus = n
    }
    if (v.memory) {
      if (typeof v.memory !== 'string' || !/^\d+[mMgG]?$/.test(v.memory)) throw new Error('invalid memory')
      out.memory = v.memory
    }
    if (v.gpus) {
      if (!['all', 'virgl', 'venus'].includes(v.gpus as string)) throw new Error('invalid gpus')
      out.gpus = v.gpus as string
    }
    out.desktop = v.desktop === true
    out.shared = v.shared === true
    for (const p of out.ports) if (!/^(\d{1,3}(\.\d{1,3}){3}:)?\d{1,5}:\d{1,5}(\/(tcp|udp))?$/.test(p)) throw new Error(`invalid port ${p}`)
    for (const e of out.env) if (!/^[A-Za-z_][A-Za-z0-9_]*=/.test(e)) throw new Error(`invalid env ${e}`)
    for (const m of out.volumes) if (!/^\/[^:]*:\/[^:]*(:(ro|rw))?$/.test(m)) throw new Error(`invalid volume ${m}`)
    return out
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const args = ['run', '-d']
    if (data.shared) args.push('--shared')
    if (data.name) args.push('--name', data.name)
    for (const p of data.ports) args.push('-p', p)
    for (const v of data.volumes) args.push('-v', v)
    for (const e of data.env) args.push('-e', e)
    if (data.cpus) args.push('--cpus', String(data.cpus))
    if (data.memory) args.push('--memory', data.memory)
    if (data.gpus) args.push('--gpus', data.gpus)
    if (data.desktop) args.push('--desktop')
    args.push('--', data.image, ...data.command)
    const result = await veynodevm<{ id: string; name: string }>(args, 30 * 60_000)
    if (!result.ok) throw new Error(result.error || 'run failed')
    return result.data ?? { id: result.stdout.trim(), name: data.name ?? '' }
  })

export const engineStatus = createServerFn({ method: 'GET' }).handler(async () => {
  const { veynodevm } = await import('./server/veynodevm')
  const result = await veynodevm<any>(['engine', 'status'])
  return result.data ?? { state: 'absent', status: result.stdout.trim() || 'not created' }
})

export const engineAction = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    if (!['start', 'stop'].includes(input as string)) throw new Error('invalid action')
    return input as 'start' | 'stop'
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['engine', data], 20 * 60_000)
    if (result.code !== 0) throw new Error(result.stderr.trim() || `engine ${data} failed`)
    return { output: result.stdout.trim() }
  })

export const openDesktop = createServerFn({ method: 'POST' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['desktop', data], 30_000)
    if (result.code !== 0) throw new Error(result.stderr.trim() || 'desktop failed')
    return { ok: true }
  })

export const systemInfo = createServerFn({ method: 'GET' }).handler(async () => {
  const cli = process.env.VEYNODEVM_CLI || 'veynodevm'
  return { cli, home: process.env.VEYNODEVM_HOME ?? null }
})
