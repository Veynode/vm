// Input checks for `machine.*` and the macOS pull, kept free of I/O so they
// run in the server functions' validators and in web/test, and the API 1
// arguments they become (docs/api.md), as crates/cli/src/machine.rs sends
// them; the daemon checks everything again.

export const NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/
const MEMORY = /^(\d{1,7})([mMgG])?$/
const DISK = /^(\d{1,4})([gGtT])?$/
const PORT = /^(\d{1,5}):(\d{1,5})$/
const USER = /^[A-Za-z0-9_][A-Za-z0-9._-]{0,63}$/
export const MACOS_IMAGE = /^ghcr\.io\/cirruslabs\/macos-[a-z0-9-]{1,64}(:[A-Za-z0-9._-]{1,128})?$/

/** Installer ISOs; read-only seed images (cloud-init, kickstart, Autounattend, virtio-win) may also be raw images. */
export const ISO_EXTENSIONS = ['.iso']
export const ATTACH_EXTENSIONS = ['.iso', '.img', '.raw']

export function machineName(input) {
  if (typeof input !== 'string' || !NAME.test(input)) throw new Error('invalid machine name: letters, digits, . _ - (up to 64, not starting with . _ -)')
  return input
}

/**
 * The shape of a host path: absolute, normalised, no control characters,
 * one of `extensions`. Whether it exists and is a regular file is checked on
 * the host (server/host-files.mjs).
 */
export function hostPath(input, extensions, label) {
  if (typeof input !== 'string' || input.length === 0 || input.length > 1024) throw new Error(`invalid ${label}: an absolute path on this Mac`)
  if (!input.startsWith('/')) throw new Error(`invalid ${label}: must be an absolute path`)
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(input)) throw new Error(`invalid ${label}: control characters`)
  if (input.split('/').some((part) => part === '..' || part === '.')) throw new Error(`invalid ${label}: no . or .. segments`)
  const lower = input.toLowerCase()
  if (!extensions.some((ext) => lower.endsWith(ext))) throw new Error(`invalid ${label}: expected ${extensions.join(', ')}`)
  return input
}

/** `4g`, `4096m`, `4096` (MiB) → the same string; 512 MiB through 128 GiB. */
export function memory(input) {
  const m = typeof input === 'string' ? MEMORY.exec(input) : null
  if (!m) throw new Error('invalid memory: e.g. 4g or 4096m')
  const mib = Number(m[1]) * (/[gG]/.test(m[2] ?? '') ? 1024 : 1)
  if (mib < 512 || mib > 128 * 1024) throw new Error('invalid memory: 512m through 128g')
  return input
}

/** `40G`, `1T`, `40` (GiB); 4 GiB through 4 TiB, as `machine create` takes it. */
export function diskSize(input) {
  const m = typeof input === 'string' ? DISK.exec(input) : null
  if (!m) throw new Error('invalid disk size: e.g. 64G')
  const gib = Number(m[1]) * (/[tT]/.test(m[2] ?? '') ? 1024 : 1)
  if (gib < 4 || gib > 4096) throw new Error('invalid disk size: 4G through 4T')
  return input
}

export function cpus(input) {
  const n = Number(input)
  if (typeof input === 'boolean' || input === '' || !Number.isInteger(n) || n < 1 || n > 32) throw new Error('invalid cpus: 1 through 32')
  return n
}

/** `HOST:GUEST` TCP ports, both 1..65535 (`machine create -p`). */
export function port(input) {
  const m = typeof input === 'string' ? PORT.exec(input) : null
  const ok = m && [m[1], m[2]].every((p) => Number(p) >= 1 && Number(p) <= 65535)
  if (!ok) throw new Error(`invalid port ${String(input)}: HOST:GUEST, e.g. 2222:22`)
  return input
}

export function sshUser(input) {
  if (typeof input !== 'string' || !USER.test(input)) throw new Error('invalid user: a Windows account name (letters, digits, . _ -)')
  return input
}

export function macosImage(input) {
  if (typeof input !== 'string' || !MACOS_IMAGE.test(input)) throw new Error('invalid macOS image: ghcr.io/cirruslabs/macos-*')
  return input
}

function list(input, max, label) {
  if (input === undefined || input === null) return []
  if (!Array.isArray(input) || input.length > max) throw new Error(`invalid ${label}`)
  return input
}

/**
 * The `machine create` form as the server takes it. Options that only QEMU
 * has are refused on Apple Virtualization here rather than by the CLI, so
 * the dialog's message names the field.
 */
export function createInput(input) {
  const v = input && typeof input === 'object' ? input : {}
  const os = v.os ?? 'linux'
  if (os !== 'linux' && os !== 'windows') throw new Error('invalid os: linux or windows')
  const out = { os, name: machineName(v.name), ports: [], attach: [] }
  if (v.iso) out.iso = hostPath(v.iso, ISO_EXTENSIONS, 'ISO path')
  if (v.diskSize) out.diskSize = diskSize(v.diskSize)
  if (v.memory) out.memory = memory(v.memory)
  if (v.cpus !== undefined && v.cpus !== '') out.cpus = cpus(v.cpus)
  const gpus = os === 'windows' ? ['neptune'] : ['virgl', 'venus']
  if (v.gpus) {
    if (!gpus.includes(v.gpus)) throw new Error(`invalid gpu for ${os}: ${gpus.join(' or ')}`)
    out.gpus = v.gpus
  }
  if (v.backend) {
    if (v.backend !== 'apple' && v.backend !== 'qemu') throw new Error('invalid backend: apple or qemu')
    out.backend = v.backend
  }
  const qemu = os === 'windows' || !!out.gpus || out.backend === 'qemu'
  if (out.backend === 'apple' && (os === 'windows' || out.gpus)) throw new Error('Apple Virtualization runs neither Windows nor a GPU; use QEMU/HVF')
  if (v.diskBus) {
    if (v.diskBus !== 'virtio' && v.diskBus !== 'nvme') throw new Error('invalid disk bus: virtio or nvme')
    if (v.diskBus === 'nvme' && !qemu) throw new Error('the NVMe disk bus is a QEMU/HVF option')
    out.diskBus = v.diskBus
  }
  if (v.monitor === true) {
    if (!qemu) throw new Error('the QMP monitor is a QEMU/HVF option')
    out.monitor = true
  }
  out.ports = list(v.ports, 16, 'ports').map(port)
  if (out.ports.length && !qemu) throw new Error('port forwards are a QEMU/HVF option (Apple Virtualization guests are reachable directly)')
  out.attach = list(v.attach, 8, 'attached images').map((p) => hostPath(p, ATTACH_EXTENSIONS, 'attached image'))
  return out
}

/** `4g`, `4096m`, `4096` → MiB, for a value `memory` accepted. */
export function memoryMiB(value) {
  const m = MEMORY.exec(value)
  return Number(m[1]) * (/[gG]/.test(m[2] ?? '') ? 1024 : 1)
}

/** `64G`, `1T`, `64` → GiB, for a value `diskSize` accepted. */
export function diskGiB(value) {
  const m = DISK.exec(value)
  return Number(m[1]) * (/[tT]/.test(m[2] ?? '') ? 1024 : 1)
}

/** `HOST:GUEST` → `{host, guest}`, for a value `port` accepted. */
export function portPair(value) {
  const [host, guest] = value.split(':').map(Number)
  return { host, guest }
}

/** The `machine.create` arguments for a checked `createInput`. */
export function createRequest(spec) {
  const args = { name: spec.name, os: spec.os }
  if (spec.gpus) args.gpu = spec.gpus
  if (spec.backend) args.backend = spec.backend
  if (spec.diskBus) args.diskBus = spec.diskBus
  if (spec.iso) args.iso = spec.iso
  if (spec.diskSize) args.diskGiB = diskGiB(spec.diskSize)
  if (spec.cpus) args.cpus = spec.cpus
  if (spec.memory) args.memoryMiB = memoryMiB(spec.memory)
  if (spec.monitor) args.monitor = true
  if (spec.attach.length) args.attach = spec.attach
  if (spec.ports.length) args.ports = spec.ports.map(portPair)
  return args
}

/** `WIDTHxHEIGHT` for `machine display`: the same 640x480 to 3840x2160 bound as the CLI. */
export function displaySize(input) {
  const m = typeof input === 'string' ? /^(\d{3,4})x(\d{3,4})$/.exec(input) : null
  const w = m ? Number(m[1]) : 0
  const h = m ? Number(m[2]) : 0
  if (!m || w < 640 || h < 480 || w > 3840 || h > 2160) throw new Error('invalid display size (640x480 to 3840x2160)')
  return `${w}x${h}`
}

export const MACHINE_ACTIONS = ['start', 'stop', 'kill', 'eject', 'rm']

/** `{action, name}` → the method and its arguments; `start` is a stream. */
export function actionRequest(input) {
  const v = input && typeof input === 'object' ? input : {}
  if (!MACHINE_ACTIONS.includes(v.action)) throw new Error('invalid action')
  const name = machineName(v.name)
  if (v.action === 'kill') return { method: 'machine.stop', args: { name, force: true } }
  return { method: `machine.${v.action}`, args: { name } }
}
