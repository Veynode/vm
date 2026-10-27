import { createServerFn } from '@tanstack/react-start'
import { actionArgs, createArgs, createInput, displaySize, hostPath, machineName, macosImage, sshUser, type MachineSpec } from './machine-input.mjs'

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
  /** `[width, height]` when not the 1280x800 default. */
  desktop_size?: [number, number] | null
  vm_id: string
  /** `KIND[:COLOR]`, set with `veynodevm icon`; absent draws the kind of container. */
  icon?: string | null
  /** "darwin" for macOS containers; empty for Linux. */
  os?: string
  /** The shared engine (`veynodevm engine`). */
  engine?: boolean
  cpus?: number
  memory_mib?: number
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
  /** `WxH`, QEMU desktops only (`--desktop-size`). */
  desktop_size?: string
  shared?: boolean
  /** GiB; macOS images grow their disk to it. */
  disk_size?: number
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

export const renameContainer = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; name?: unknown }
    return { id: id(value?.id), name: id(value?.name) }
  })
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    return must<{ id: string; name: string }>(['rename', data.id, data.name])
  })

/** What the Hardware dialog changes; an absent field is kept. */
export interface HardwareChanges {
  cpus?: number
  /** MiB. */
  memory_mib?: number
  /** GiB; only grows. */
  disk_gib?: number
  /** `''` drops the GPU; `virgl`, `venus`, or `neptune` on a Windows VM. */
  gpus?: string
  desktop?: boolean
  /** `WxH`, QEMU desktops only. */
  desktop_size?: string
  /** `HOST:GUEST[/udp]`; empty publishes none. */
  ports?: string[]
  /** Empty goes back to the default servers. */
  dns?: string[]
  /** 0 goes back to the default. */
  mtu?: number
  new_mac?: boolean
}

const PORT = /^(\d{1,3}(\.\d{1,3}){3}:)?\d{1,5}:\d{1,5}(\/(tcp|udp))?$/

function int(value: unknown, min: number, max: number, label: string): number | undefined {
  if (value === undefined) return undefined
  if (!Number.isInteger(value) || (value as number) < min || (value as number) > max) throw new Error(`invalid ${label}`)
  return value as number
}

/** Checked changes as `update` / `machine update` flags. */
function hardwareArgs(input: unknown, machine: boolean): string[] {
  const v = (input ?? {}) as Record<string, unknown>
  const args: string[] = []
  const cpus = int(v.cpus, 1, 128, 'CPU count')
  if (cpus !== undefined) args.push('--cpus', String(cpus))
  const memory = int(v.memory_mib, 256, 1 << 20, 'memory')
  if (memory !== undefined) args.push('--memory', `${memory}m`)
  const disk = int(v.disk_gib, 1, 4096, 'disk size')
  if (disk !== undefined) args.push('--disk-size', `${disk}G`)
  if (v.gpus !== undefined) {
    if (!['', 'virgl', 'venus', ...(machine ? ['neptune'] : [])].includes(v.gpus as string)) throw new Error('invalid GPU')
    args.push('--gpus', (v.gpus as string) || 'none')
  }
  if (!machine && v.desktop !== undefined) {
    if (typeof v.desktop !== 'boolean') throw new Error('invalid desktop')
    args.push('--desktop', v.desktop ? 'on' : 'off')
  }
  if (!machine && v.desktop_size !== undefined) {
    if (!(typeof v.desktop_size === 'string' && /^\d{3,4}x\d{3,4}$/.test(v.desktop_size))) throw new Error('invalid desktop size')
    args.push('--desktop-size', v.desktop_size)
  }
  if (v.ports !== undefined) {
    const ports = list(v.ports, 64, 'ports')
    if (ports.some((p) => !(machine ? /^\d{1,5}:\d{1,5}$/ : PORT).test(p))) throw new Error('invalid port')
    args.push(...(ports.length ? ports.flatMap((p) => ['--publish', p]) : ['--publish', 'none']))
  }
  if (!machine && v.dns !== undefined) {
    const dns = list(v.dns, 3, 'DNS servers')
    if (dns.some((d) => !/^[0-9a-fA-F.:]{2,45}$/.test(d))) throw new Error('invalid DNS server')
    args.push(...(dns.length ? dns.flatMap((d) => ['--dns', d]) : ['--dns', 'none']))
  }
  if (!machine && v.mtu !== undefined) {
    const mtu = v.mtu === 0 ? 0 : int(v.mtu, 576, 9000, 'MTU')
    args.push('--mtu', mtu ? String(mtu) : 'default')
  }
  if (v.new_mac === true) args.push('--new-mac')
  if (!args.length) throw new Error('nothing to change')
  return args
}

/** `veynodevm update`: a container's hardware, in one re-registration of its VM. */
export const updateContainer = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; changes?: unknown; restart?: unknown }
    return { id: id(value?.id), args: hardwareArgs(value?.changes, false), restart: value?.restart === true }
  })
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    // A restart stops the guest first (up to 30 s under systemd), then boots it.
    return must<{ id: string }>(['update', ...data.args, ...(data.restart ? ['--restart'] : []), data.id], 120_000)
  })

const ICON = /^(monitor|laptop|vm|box|server|terminal|cpu|globe|folder|code)(:(blue|teal|green|yellow|orange|pink|purple|black))?$/

export const setContainerIcon = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; icon?: unknown }
    if (value?.icon !== null && !(typeof value?.icon === 'string' && ICON.test(value.icon))) throw new Error('invalid icon')
    return { id: id(value.id), icon: value.icon as string | null }
  })
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    await must(['icon', data.id, data.icon ?? '--reset'])
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
    if (v.disk_size !== undefined) {
      const n = Number(v.disk_size)
      if (!Number.isInteger(n) || n < 8 || n > 4096) throw new Error('invalid disk size')
      out.disk_size = n
    }
    out.desktop = v.desktop === true
    if (typeof v.desktop_size === 'string' && v.desktop_size) {
      if (!/^\d{3,4}x\d{3,4}$/.test(v.desktop_size)) throw new Error('invalid desktop size')
      out.desktop_size = v.desktop_size
    }
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
    if (data.disk_size) args.push('--disk-size', String(data.disk_size))
    if (data.gpus) args.push('--gpus', data.gpus)
    if (data.desktop) args.push('--desktop')
    if (data.desktop && data.desktop_size) args.push('--desktop-size', data.desktop_size)
    args.push('--', data.image, ...data.command)
    const result = await veynodevm<{ id: string; name: string }>(args, 30 * 60_000)
    if (!result.ok) throw new Error(result.error || 'run failed')
    return result.data ?? { id: result.stdout.trim(), name: data.name ?? '' }
  })

export interface DaemonStatus {
  running: boolean
  label: string
  log: string
  state_dir?: string
  memory_budget_mib?: number
  assigned_memory_mib?: number
  qemu_registered?: boolean
  /** The cores a VM can be given at most. */
  host_cpus?: number
}

/** The control daemon (`veynodevm daemon`): it owns every container's and VM's micro-VM. */
export const daemonStatus = createServerFn({ method: 'GET' }).handler(async () => {
  const { veynodevm } = await import('./server/veynodevm')
  const result = await veynodevm<DaemonStatus>(['daemon', 'status'])
  if (!result.data) throw new Error(result.stderr.trim() || 'daemon status failed')
  return result.data
})

export const daemonAction = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    if (!['start', 'stop'].includes(input as string)) throw new Error('invalid action')
    return input as 'start' | 'stop'
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['daemon', data], 60_000)
    if (result.code !== 0) throw new Error(result.stderr.trim().replace(/^veynodevm: /, '') || `daemon ${data} failed`)
    return { ok: true }
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

export const closeDesktop = createServerFn({ method: 'POST' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['desktop', '--close', data], 30_000)
    if (result.code !== 0) throw new Error(result.stderr.trim() || 'desktop --close failed')
    return { ok: true }
  })

export const systemInfo = createServerFn({ method: 'GET' }).handler(async () => {
  const { veynodevm } = await import('./server/veynodevm')
  const cli = process.env.VEYNODEVM_CLI || 'veynodevm'
  // `veynodevm version` prints `veynodevm X.Y.Z`.
  const version = (await veynodevm(['version'])).stdout.trim().replace(/^veynodevm\s+/, '') || null
  return { cli, home: process.env.VEYNODEVM_HOME ?? null, version }
})

/* ----- Machines (`veynodevm machine`) ---------------------------------------- */

export interface MachineRecord {
  name: string
  /** Stays the same across renames: the address of its pages. */
  id: string
  backend: 'qemu-hvf' | 'apple-virtualization'
  os: 'linux' | 'windows'
  iso?: string
  disk_gib: number
  created_at: number
  /** `KIND[:COLOR]`, as a container's (`veynodevm machine icon`). */
  icon?: string
}
export interface MachineRow {
  machine: MachineRecord
  state: string
  disk_used_bytes: number
  /** From the daemon's VM configuration; absent while the daemon is down. */
  cpus?: number
  memory_mib?: number
  /** QEMU's console broker is up, so the browser desktop can stream it. */
  desktop?: boolean
  /** It has an SSH login (`machine ssh-key`) and a forward to guest port 22: the terminal. */
  terminal?: boolean
}

function stderrOf(result: { error?: string; stderr: string }, fallback: string) {
  return (result.error || result.stderr.trim() || fallback).replace(/^veynodevm: /, '')
}

export const listMachines = createServerFn({ method: 'GET' }).handler(async () => {
  const { must, veynodevm } = await import('./server/veynodevm')
  const { sshLogin, veynodeHome } = await import('./server/host-files.mjs')
  const { join } = await import('node:path')
  const [rows, vms] = await Promise.all([must<MachineRow[]>(['machine', 'ls']), veynodevm<any>(['vm', 'ls'])])
  const list: any[] = Array.isArray(vms.data) ? vms.data : Array.isArray(vms.data?.vms) ? vms.data.vms : []
  return Promise.all(
    rows.map(async (row) => {
      const vm = list.find((v) => (v?.config?.id ?? v?.id) === row.machine.name)
      const config = vm?.config ?? vm
      const ssh = sshForward(config) ? await sshLogin(join(veynodeHome(), 'machines', row.machine.name)) : null
      return { ...row, cpus: config?.cpus, memory_mib: config?.memory_mib, desktop: !!vm?.runtime?.console_socket, terminal: !!(ssh?.user && ssh.key) }
    }),
  )
})

/** `windows_guest.rs` `Ssh::locate`: SSH goes through a TCP forward to guest port 22. */
export const sshForward = (config: any) => (config?.ports ?? []).some((p: any) => p?.guest_port === 22 && (p.protocol ?? 'tcp') === 'tcp')

export const inspectMachine = createServerFn({ method: 'GET' })
  .validator((input: unknown) => machineName(input))
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    const { sshLogin, veynodeHome } = await import('./server/host-files.mjs')
    const { join } = await import('node:path')
    const info = await must<{ machine: MachineRecord; vm: any }>(['machine', 'inspect', data])
    // Windows shows and sets its login; any machine with one gets a terminal.
    const ssh = info.machine.os === 'windows' || sshForward(info.vm?.config) ? await sshLogin(join(veynodeHome(), 'machines', info.machine.name)) : null
    return { ...info, ssh }
  })

export const machineAction = createServerFn({ method: 'POST' })
  .validator((input: unknown) => actionArgs(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(data, 120_000)
    if (!result.ok) throw new Error(stderrOf(result, `machine ${data[1]} failed`))
    return { ok: true }
  })

export const createMachine = createServerFn({ method: 'POST' })
  .validator((input: unknown): MachineSpec => createInput(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const { regularFile } = await import('./server/host-files.mjs')
    if (data.iso) await regularFile(data.iso, 'Installer ISO')
    for (const path of data.attach) await regularFile(path, 'Attached image')
    const result = await veynodevm(createArgs(data), 10 * 60_000)
    if (!result.ok) throw new Error(stderrOf(result, 'machine create failed'))
    const { must } = await import('./server/veynodevm')
    const info = await must<{ machine: MachineRecord }>(['machine', 'inspect', data.name])
    return { name: data.name, id: info.machine.id }
  })

export const setMachineIcon = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { name?: unknown; icon?: unknown }
    if (value?.icon !== null && !(typeof value?.icon === 'string' && ICON.test(value.icon))) throw new Error('invalid icon')
    return { name: machineName(value.name), icon: value.icon as string | null }
  })
  .handler(async ({ data }) => {
    const { must } = await import('./server/veynodevm')
    await must(['machine', 'icon', data.name, data.icon ?? '--reset'])
    return { ok: true }
  })

export const renameMachine = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { name?: unknown; new_name?: unknown }
    return { name: machineName(value?.name), new_name: machineName(value?.new_name) }
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['machine', 'rename', data.name, data.new_name], 60_000)
    if (!result.ok) throw new Error(stderrOf(result, 'machine rename failed'))
    return { name: data.new_name }
  })

/** `veynodevm machine update`: a stopped VM's hardware. */
export const updateMachine = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { name?: unknown; changes?: unknown }
    return { name: machineName(value?.name), args: hardwareArgs(value?.changes, true) }
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['machine', 'update', data.name, ...data.args], 60_000)
    if (!result.ok) throw new Error(stderrOf(result, 'machine update failed'))
    return { name: data.name }
  })

/** `veynodevm machine display`: a running Windows machine's resolution, through its SSH login. */
export const setMachineDisplay = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const v = (input ?? {}) as Record<string, unknown>
    return { name: machineName(v.name), size: displaySize(v.size) }
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const result = await veynodevm(['machine', 'display', data.name, data.size], 60_000)
    if (!result.ok) throw new Error(stderrOf(result, 'machine display failed'))
    return { ok: true }
  })

export const setMachineSshKey = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const v = (input ?? {}) as Record<string, unknown>
    // Any file name: OpenSSH keys usually have no extension.
    return { name: machineName(v.name), user: sshUser(v.user), key: hostPath(v.key, [''], 'key path') }
  })
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const { regularFile } = await import('./server/host-files.mjs')
    // The CLI reads and copies the key; it never passes through this process.
    await regularFile(data.key, 'Private key')
    const result = await veynodevm(['machine', 'ssh-key', data.name, '--user', data.user, '--key', data.key])
    if (!result.ok) throw new Error(stderrOf(result, 'machine ssh-key failed'))
    return { ok: true }
  })

/* ----- macOS images --------------------------------------------------------- */

/** Whether a macOS image is pulled, free space where it would go, and its pull if one ran. */
export const macosImageStatus = createServerFn({ method: 'GET' })
  .validator((input: unknown) => macosImage(input))
  .handler(async ({ data }) => {
    const { veynodevm } = await import('./server/veynodevm')
    const { freeBytes, veynodeHome } = await import('./server/host-files.mjs')
    const { pullState } = await import('./server/pulls')
    const images = await veynodevm<ImageRow[]>(['images'])
    const pulled = (images.data ?? []).some((i) => i.reference === data || i.reference === `${data}:latest`)
    return { pulled, free_bytes: await freeBytes(veynodeHome()), pull: pullState(data) }
  })

export const startMacosPull = createServerFn({ method: 'POST' })
  .validator((input: unknown) => macosImage(input))
  .handler(async ({ data }) => {
    const { startPull } = await import('./server/pulls')
    return startPull(data)
  })

/** The current name of a container or machine, whose pages are addressed by id; the id where it cannot be read. */
export async function nameOf(id: string, machine: boolean): Promise<string> {
  try {
    return machine ? (await inspectMachine({ data: id })).machine.name : (await inspectContainer({ data: id })).container.name
  } catch {
    return id
  }
}
