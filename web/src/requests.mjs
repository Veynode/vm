// API 1 arguments (docs/api.md) for the forms that are not a machine's
// (machine-input.mjs has those): `container.run`, `container.update` and
// `machine.update` changes, as the CLI sends them for its flags. Free of I/O,
// so the server functions' validators and web/test share them.

const PORT = /^(\d{1,3}(\.\d{1,3}){3}:)?\d{1,5}:\d{1,5}(\/(tcp|udp))?$/
const MACHINE_PORT = /^\d{1,5}:\d{1,5}$/
const MEMORY = /^(\d+)([mMgG])?$/

function int(value, min, max, label) {
  if (value === undefined) return undefined
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`invalid ${label}`)
  return value
}

function list(input, max, label) {
  if (input === undefined || input === null) return []
  if (!Array.isArray(input) || input.length > max) throw new Error(`invalid ${label}`)
  return input
}

/** `WxH` → `[w, h]`. */
function size(value) {
  return value.split('x').map(Number)
}

/**
 * The Hardware dialog's changes as `container.update` `changes` or
 * `machine.update` arguments (without `name`); an absent field is kept.
 */
export function hardwareChanges(input, machine) {
  const v = input && typeof input === 'object' ? input : {}
  const out = {}
  const cpus = int(v.cpus, 1, 128, 'CPU count')
  if (cpus !== undefined) out.cpus = cpus
  const memory = int(v.memory_mib, 256, 1 << 20, 'memory')
  if (memory !== undefined) out.memoryMiB = memory
  const disk = int(v.disk_gib, 1, 4096, 'disk size')
  if (disk !== undefined) out.diskGiB = disk
  if (v.gpus !== undefined) {
    if (!['', 'virgl', 'venus', ...(machine ? ['neptune'] : [])].includes(v.gpus)) throw new Error('invalid GPU')
    if (machine) out.gpu = v.gpus || null
    else out.gpus = v.gpus || 'none'
  }
  if (!machine && v.desktop !== undefined) {
    if (typeof v.desktop !== 'boolean') throw new Error('invalid desktop')
    out.desktop = v.desktop
  }
  if (!machine && v.desktop_size !== undefined) {
    if (!(typeof v.desktop_size === 'string' && /^\d{3,4}x\d{3,4}$/.test(v.desktop_size))) throw new Error('invalid desktop size')
    out.desktopSize = size(v.desktop_size)
  }
  if (machine && v.audio !== undefined) {
    if (typeof v.audio !== 'boolean') throw new Error('invalid audio')
    out.audio = v.audio
  }
  if (v.ports !== undefined) {
    const ports = list(v.ports, 64, 'ports')
    if (ports.some((p) => typeof p !== 'string' || !(machine ? MACHINE_PORT : PORT).test(p))) throw new Error('invalid port')
    out.ports = machine
      ? ports.map((p) => {
          const [host, guest] = p.split(':').map(Number)
          return { host, guest }
        })
      : ports
  }
  if (!machine && v.dns !== undefined) {
    const dns = list(v.dns, 3, 'DNS servers')
    if (dns.some((d) => typeof d !== 'string' || !/^[0-9a-fA-F.:]{2,45}$/.test(d))) throw new Error('invalid DNS server')
    out.dns = dns
  }
  if (!machine && v.mtu !== undefined) out.mtu = v.mtu === 0 ? 0 : int(v.mtu, 576, 9000, 'MTU')
  if (v.new_mac === true) out.newMac = true
  if (!Object.keys(out).length) throw new Error('nothing to change')
  return out
}

/** A checked `RunInput` as `container.run` arguments: detached and quiet, like `veynodevm --json run -d`. */
export function runRequest(v) {
  const args = {
    image: v.image,
    command: v.command,
    detach: true,
    quiet: true,
    ports: v.ports,
    volumes: v.volumes,
    env: v.env,
    desktop: v.desktop === true,
  }
  if (v.name) args.name = v.name
  if (v.cpus) args.cpus = v.cpus
  if (v.memory) {
    const m = MEMORY.exec(v.memory)
    args.memoryMiB = Number(m[1]) * (/[gG]/.test(m[2] ?? '') ? 1024 : 1)
  }
  if (v.disk_size) args.diskGiB = v.disk_size
  if (v.gpus) args.gpus = v.gpus
  if (v.desktop && v.desktop_size) args.desktopSize = size(v.desktop_size)
  return args
}

export const LABEL = 'dev.veynode.veynodevm'

/**
 * The launchd label of the daemon for `root` (crates/cli/src/daemon.rs
 * `label_for`): `LABEL` for the default home, else `LABEL.` and the FNV-1a
 * hash of the path's bytes.
 */
export function daemonLabel(root, defaultRoot) {
  if (root === defaultRoot) return LABEL
  let hash = 0xcbf29ce484222325n
  for (const byte of Buffer.from(root, 'utf8')) hash = ((hash ^ BigInt(byte)) * 0x100000001b3n) & 0xffffffffffffffffn
  return `${LABEL}.${hash.toString(16).padStart(16, '0')}`
}
