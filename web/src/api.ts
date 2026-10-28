import { createServerFn } from '@tanstack/react-start'
import { actionRequest, createInput, createRequest, displaySize, hostPath, machineName, macosImage, sshUser, type MachineSpec } from './machine-input.mjs'
import { daemonLabel, hardwareChanges, runRequest } from './requests.mjs'

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
  /** `systemd` for `--init systemd`, which has no interactive session. */
  init?: string | null
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
  const { api } = await import('./server/veynodevm')
  return api<ContainerRow[]>('container.ls', { all: true })
})

export const listImages = createServerFn({ method: 'GET' }).handler(async () => {
  const { api } = await import('./server/veynodevm')
  return api<ImageRow[]>('image.ls')
})

export const inspectContainer = createServerFn({ method: 'GET' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    try {
      return await api<any>('container.inspect', { name: data })
    } catch (error: any) {
      if (error?.code !== 'not_found') throw error
      return api<any>('image.inspect', { reference: data })
    }
  })

export const containerLogs = createServerFn({ method: 'GET' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { collect } = await import('./server/veynodevm')
    const { stdout, stderr } = await collect('container.logs', { name: data })
    const text = stdout + stderr
    return text.length > 256 * 1024 ? text.slice(-256 * 1024) : text
  })

export const containerAction = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { action?: unknown; id?: unknown }
    if (!['start', 'stop', 'rm'].includes(value?.action as string)) throw new Error('invalid action')
    return { action: value.action as 'start' | 'stop' | 'rm', id: id(value.id) }
  })
  .handler(async ({ data }) => {
    const { api, ensureDaemon } = await import('./server/veynodevm')
    if (data.action === 'start') await ensureDaemon()
    const args = data.action === 'rm' ? { name: data.id, force: true } : { name: data.id }
    await api(`container.${data.action}`, args, 120_000)
    return { ok: true }
  })

export const renameContainer = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; name?: unknown }
    return { id: id(value?.id), name: id(value?.name) }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    const row = await api<{ id: string; name: string }>('container.rename', { name: data.id, newName: data.name })
    return { id: row.id, name: row.name }
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
  /** QEMU VMs only: a sound card (a desktop container always has one). */
  audio?: boolean
  /** `HOST:GUEST[/udp]`; empty publishes none. */
  ports?: string[]
  /** Empty goes back to the default servers. */
  dns?: string[]
  /** 0 goes back to the default. */
  mtu?: number
  new_mac?: boolean
}

/** `container.update`: a container's hardware, in one re-registration of its VM. */
export const updateContainer = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; changes?: unknown; restart?: unknown }
    return { id: id(value?.id), changes: hardwareChanges(value?.changes, false), restart: value?.restart === true }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    // A restart stops the guest first (up to 30 s under systemd), then boots it.
    return api<{ id: string }>('container.update', { name: data.id, changes: data.changes, restart: data.restart }, 120_000)
  })

const ICON = /^(monitor|laptop|vm|box|server|terminal|cpu|globe|folder|code)(:(blue|teal|green|yellow|orange|pink|purple|black))?$/

export const setContainerIcon = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { id?: unknown; icon?: unknown }
    if (value?.icon !== null && !(typeof value?.icon === 'string' && ICON.test(value.icon))) throw new Error('invalid icon')
    return { id: id(value.id), icon: value.icon as string | null }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    await api('container.icon', { name: data.id, icon: data.icon })
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
    const { collect } = await import('./server/veynodevm')
    const { result, stdout, stderr } = await collect<{ exitCode: number; timedOut: boolean }>('container.exec', {
      name: data.id,
      argv: data.argv,
    })
    return { exit_code: result.exitCode, stdout, stderr, timed_out: result.timedOut }
  })

export const pullImage = createServerFn({ method: 'POST' })
  .validator((input: unknown) => image(input))
  .handler(async ({ data }) => {
    const { collect } = await import('./server/veynodevm')
    return (await collect<ImageRow>('image.pull', { reference: data })).result
  })

export const removeImage = createServerFn({ method: 'POST' })
  .validator((input: unknown) => image(input))
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    await api('image.rm', { reference: data })
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
    const { collect, ensureDaemon, veynodevm } = await import('./server/veynodevm')
    if (!data.shared) {
      await ensureDaemon()
      const { result } = await collect<{ id: string; name: string }>('container.run', runRequest(data))
      return { id: result.id, name: result.name }
    }
    // The shared engine's containers are the user's docker client's (`veynodevm run --shared`).
    const args = ['run', '-d', '--shared']
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

/**
 * The control daemon: it owns every container's and VM's micro-VM. Its
 * budget comes over API 1; launchd's label and the log are this home's
 * (`veynodevm daemon status`).
 */
export const daemonStatus = createServerFn({ method: 'GET' }).handler(async (): Promise<DaemonStatus> => {
  const { api } = await import('./server/veynodevm')
  const { veynodeHome } = await import('./server/host-files.mjs')
  const { availableParallelism, homedir } = await import('node:os')
  const { join } = await import('node:path')
  const home = veynodeHome()
  const base = {
    label: daemonLabel(home, join(process.env.HOME || homedir(), 'Library/Application Support/VeynodeVM')),
    log: join(home, 'daemon.log'),
    host_cpus: availableParallelism(),
  }
  try {
    const c = await api<any>('capabilities', {}, 10_000)
    return {
      ...base,
      running: true,
      state_dir: join(home, 'state'),
      memory_budget_mib: c?.resource_admission?.vm_memory_budget_mib,
      assigned_memory_mib: c?.resource_admission?.active_assigned_memory_mib,
      qemu_registered: c?.backends?.['qemu-hvf']?.registered,
    }
  } catch {
    return { ...base, running: false }
  }
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
  const { api } = await import('./server/veynodevm')
  try {
    return await api<any>('engine.status')
  } catch (error: any) {
    if (error?.code === 'not_found') return { state: 'absent', status: 'engine not created (veynodevm engine start)' }
    throw error
  }
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
    const { api } = await import('./server/veynodevm')
    await api('desktop.open', { name: data }, 30_000)
    return { ok: true }
  })

export const closeDesktop = createServerFn({ method: 'POST' })
  .validator((input: unknown) => id(input))
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    await api('desktop.close', { name: data }, 30_000)
    return { ok: true }
  })

export const systemInfo = createServerFn({ method: 'GET' }).handler(async () => {
  const { veynodevm } = await import('./server/veynodevm')
  const cli = process.env.VEYNODEVM_CLI || 'veynodevm'
  // `veynodevm version` prints `veynodevm X.Y.Z`.
  const version = (await veynodevm(['version'])).stdout.trim().replace(/^veynodevm\s+/, '') || null
  return { cli, home: process.env.VEYNODEVM_HOME ?? null, version }
})

/* ----- Machines (`machine.*`) ------------------------------------------------ */

export interface MachineRecord {
  name: string
  /** Stays the same across renames: the address of its pages. */
  id: string
  backend: 'qemu-hvf' | 'apple-virtualization'
  os: 'linux' | 'windows' | 'macos'
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

export const listMachines = createServerFn({ method: 'GET' }).handler(async () => {
  const { api } = await import('./server/veynodevm')
  const { sshLogin, veynodeHome } = await import('./server/host-files.mjs')
  const { join } = await import('node:path')
  const machines = await api<any[]>('machine.ls')
  return Promise.all(
    machines.map(async (m): Promise<MachineRow> => {
      const config = m.vm?.config
      const ssh = sshForward(config) ? await sshLogin(join(veynodeHome(), 'machines', m.name)) : null
      return {
        machine: m.record,
        state: m.vmState,
        disk_used_bytes: m.diskUsedBytes,
        cpus: config?.cpus,
        memory_mib: config?.memory_mib,
        desktop: !!m.vm?.runtime?.console_socket,
        terminal: !!(ssh?.user && ssh.key),
      }
    }),
  )
})

/** `windows_guest.rs` `Ssh::locate`: SSH goes through a TCP forward to guest port 22. */
export const sshForward = (config: any) => (config?.ports ?? []).some((p: any) => p?.guest_port === 22 && (p.protocol ?? 'tcp') === 'tcp')

export const inspectMachine = createServerFn({ method: 'GET' })
  .validator((input: unknown) => machineName(input))
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    const { sshLogin, veynodeHome } = await import('./server/host-files.mjs')
    const { join } = await import('node:path')
    const m = await api<any>('machine.inspect', { name: data })
    const info: { machine: MachineRecord; vm: any } = { machine: m.record, vm: m.vm }
    // Windows shows and sets its login; any machine with one gets a terminal.
    const ssh = info.machine.os === 'windows' || sshForward(info.vm?.config) ? await sshLogin(join(veynodeHome(), 'machines', info.machine.name)) : null
    return { ...info, ssh }
  })

export const machineAction = createServerFn({ method: 'POST' })
  .validator((input: unknown) => actionRequest(input))
  .handler(async ({ data }) => {
    const { api, collect, ensureDaemon } = await import('./server/veynodevm')
    if (data.method === 'machine.start') {
      await ensureDaemon()
      await collect(data.method, data.args)
    } else {
      await api(data.method, data.args, 120_000)
    }
    return { ok: true }
  })

export const createMachine = createServerFn({ method: 'POST' })
  .validator((input: unknown): MachineSpec => createInput(input))
  .handler(async ({ data }) => {
    const { collect, ensureDaemon } = await import('./server/veynodevm')
    const { regularFile } = await import('./server/host-files.mjs')
    if (data.iso) await regularFile(data.iso, 'Installer ISO')
    for (const path of data.attach) await regularFile(path, 'Attached image')
    await ensureDaemon()
    const { result } = await collect<{ name: string; id: string }>('machine.create', createRequest(data))
    return { name: result.name, id: result.id }
  })

export const setMachineIcon = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { name?: unknown; icon?: unknown }
    if (value?.icon !== null && !(typeof value?.icon === 'string' && ICON.test(value.icon))) throw new Error('invalid icon')
    return { name: machineName(value.name), icon: value.icon as string | null }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    await api('machine.icon', { name: data.name, icon: data.icon })
    return { ok: true }
  })

export const renameMachine = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { name?: unknown; new_name?: unknown }
    return { name: machineName(value?.name), new_name: machineName(value?.new_name) }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    const machine = await api<{ name: string }>('machine.rename', { name: data.name, newName: data.new_name }, 60_000)
    return { name: machine.name }
  })

/** `machine.update`: a stopped VM's hardware. */
export const updateMachine = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const value = input as { name?: unknown; changes?: unknown }
    return { name: machineName(value?.name), changes: hardwareChanges(value?.changes, true) }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    await api('machine.update', { ...data.changes, name: data.name }, 60_000)
    return { name: data.name }
  })

/** `machine.display`: a running Windows machine's resolution, through its SSH login. */
export const setMachineDisplay = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const v = (input ?? {}) as Record<string, unknown>
    return { name: machineName(v.name), size: displaySize(v.size) }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    const [width, height] = data.size.split('x').map(Number)
    await api('machine.display', { name: data.name, width, height }, 60_000)
    return { ok: true }
  })

export const setMachineSshKey = createServerFn({ method: 'POST' })
  .validator((input: unknown) => {
    const v = (input ?? {}) as Record<string, unknown>
    // Any file name: OpenSSH keys usually have no extension.
    return { name: machineName(v.name), user: sshUser(v.user), key: hostPath(v.key, [''], 'key path') }
  })
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    const { regularFile } = await import('./server/host-files.mjs')
    // The daemon reads and copies the key; it never passes through this process.
    await regularFile(data.key, 'Private key')
    await api('machine.ssh_key', { name: data.name, user: data.user, key: data.key })
    return { ok: true }
  })

/* ----- macOS images --------------------------------------------------------- */

/** Whether a macOS image is pulled, free space where it would go, and its pull if one ran. */
export const macosImageStatus = createServerFn({ method: 'GET' })
  .validator((input: unknown) => macosImage(input))
  .handler(async ({ data }) => {
    const { api } = await import('./server/veynodevm')
    const { freeBytes, veynodeHome } = await import('./server/host-files.mjs')
    const { pullState } = await import('./server/pulls')
    const images = await api<ImageRow[]>('image.ls').catch(() => [] as ImageRow[])
    const pulled = images.some((i) => i.reference === data || i.reference === `${data}:latest`)
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
