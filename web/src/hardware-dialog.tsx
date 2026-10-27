import { useEffect, useState, type ReactNode } from 'react'
import { useRouter } from '@tanstack/react-router'
import { NumberField } from '@console/components/ui/NumberField'
import { Button } from '@ui/components/ui/button'
import { IconButton } from '@ui/components/ui/icon-button'
import { Input } from '@ui/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@ui/components/ui/select'
import { Switch } from '@ui/components/ui/switch'
import { Cpu, HardDrive, MemoryStick, Monitor, Network, Plus, Power, X, Zap } from '@ui/icons'
import { Field } from '@ui/settings/field'
import { InfoTip } from '@ui/settings/info-tip'
import { Notice } from '@ui/settings/notice'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup, SettingsSection } from '@ui/settings/settings-section'
import { daemonStatus, machineAction, updateContainer, updateMachine, type DaemonStatus, type HardwareChanges } from './api'
import { DESKTOP_SIZES, GPUS } from './run-dialog'
import { FormDialog, size, useToastAction } from './ui'

/**
 * A container's or VM's hardware as the Hardware section and dialog show it,
 * read from `inspect` / `machine inspect`. `gpu` is the dialog's choice
 * (`''`, `virgl`, `venus`, `neptune`); `ports` are `HOST:GUEST[/udp]`.
 */
export interface Hardware {
  kind: 'container' | 'machine'
  /** The id `update` / `machine update` takes. */
  id: string
  name: string
  running: boolean
  os: 'linux' | 'windows' | 'darwin'
  backend: string
  /** The shared engine: managed on Engine. */
  engine?: boolean
  cpus: number
  memory_mib: number
  disk_gib: number
  disk_used_bytes?: number
  gpu: string
  /** The renderer the guest reported. */
  renderer?: string | null
  desktop?: boolean
  /** `WxH`, `''` for the 1280x800 default. */
  desktop_size?: string
  ports: string[]
  /** Containers only; empty is the default servers. */
  dns?: string[]
  mtu?: number | null
  mac?: string | null
}

const GIB = 1024

function gpuChoice(profile: string | null | undefined): string {
  if (!profile) return ''
  if (profile.startsWith('venus')) return 'venus'
  if (profile.startsWith('neptune')) return 'neptune'
  return 'virgl'
}

function port(p: { host_address?: string; host_port: number; guest_port: number; protocol?: string }) {
  const host = p.host_address && p.host_address !== '127.0.0.1' ? `${p.host_address}:` : ''
  return `${host}${p.host_port}:${p.guest_port}${p.protocol === 'udp' ? '/udp' : ''}`
}

/** A container's hardware from `inspect`. */
export function containerHardware(info: any, running: boolean): Hardware {
  const c = info.container
  return {
    kind: 'container',
    id: c.id,
    name: c.name,
    running,
    os: c.os === 'darwin' ? 'darwin' : 'linux',
    backend: c.backend,
    engine: c.engine,
    cpus: c.cpus,
    memory_mib: c.memory_mib,
    disk_gib: Math.ceil((info.disk_bytes ?? 16 * 2 ** 30) / 2 ** 30),
    disk_used_bytes: info.disk_used_bytes,
    gpu: gpuChoice(c.gpus),
    renderer: info.gpu,
    desktop: c.desktop,
    desktop_size: c.desktop_size ? `${c.desktop_size[0]}x${c.desktop_size[1]}` : '',
    ports: c.ports.map(port),
    dns: Array.isArray(info.dns) ? info.dns : [],
    mtu: typeof info.mtu === 'number' ? info.mtu : null,
    mac: c.mac_address ?? null,
  }
}

/** A VM's hardware from `machine inspect` (`config` is the daemon's VM configuration). */
export function machineHardware(m: any, config: any, running: boolean, used?: number): Hardware {
  return {
    kind: 'machine',
    id: m.name,
    name: m.name,
    running,
    os: m.os,
    backend: m.backend,
    cpus: config.cpus ?? 0,
    memory_mib: config.memory_mib ?? 0,
    disk_gib: m.disk_gib,
    disk_used_bytes: used,
    gpu: gpuChoice(config.qemu_graphics_profile),
    ports: (config.ports ?? []).map(port),
    mac: config.mac_address ?? null,
  }
}

const memoryText = (mib: number) => (mib % GIB === 0 ? `${mib / GIB} GiB` : mib > GIB ? `${(mib / GIB).toFixed(1)} GiB` : `${mib} MiB`)

/** The GPUs this target can have, by value; Apple Virtualization VMs have none. */
function gpuChoices(hw: Hardware): Record<string, string> | null {
  if (hw.kind === 'container') return GPUS
  if (hw.backend !== 'qemu-hvf') return null
  return hw.os === 'windows' ? { '': 'Basic display', neptune: 'Neptune — DirectX (beta)' } : GPUS
}

/**
 * The Hardware section of a detail page: what the VM has, read-only, and Edit
 * for the dialog that changes it.
 */
export function HardwareSection({ hw }: { hw: Hardware }) {
  const [editing, setEditing] = useState(false)
  const qemu = hw.backend === 'qemu-hvf'
  const gpus = gpuChoices(hw)
  const network = ['NAT', ...(hw.ports.length ? [hw.ports.map((p) => p.replace(/^(.*):(\d+)/, '$1→$2')).join(', ')] : [])].join(' · ')
  const gpuText = hw.os === 'darwin' ? 'Apple Paravirtual (Metal)' : gpus ? (gpus[hw.gpu] ?? hw.gpu) : 'None'
  return (
    <SettingsSection
      title="Hardware"
      actions={
        // The engine's hardware is the engine's own (Engine page).
        !hw.engine && (
          <Button size="sm" variant="secondary" onClick={() => setEditing(true)}>
            Edit
          </Button>
        )
      }
    >
      <SettingsGroup>
        <SettingsRow
          icon={Cpu}
          label="Processors and memory"
          value={hw.cpus ? `${hw.cpus} ${hw.cpus === 1 ? 'core' : 'cores'} · ${memoryText(hw.memory_mib)}` : '—'}
        />
        <SettingsRow icon={HardDrive} label="Disk" value={`${hw.disk_gib} GB${hw.disk_used_bytes !== undefined ? ` · ${size(hw.disk_used_bytes)} used` : ''}`} />
        <SettingsRow icon={Network} label="Network" value={<span title={network}>{network}</span>} />
        {/* What the guest reported, else what the GPU runs on. */}
        <SettingsRow icon={Zap} label="GPU" hint={(hw.gpu && hw.renderer) || `Runs on ${qemu ? 'QEMU/HVF' : 'Apple Virtualization'}`} value={gpuText} />
        {hw.kind === 'container' && hw.os !== 'darwin' && (
          <SettingsRow icon={Monitor} label="Display" value={hw.desktop ? (DESKTOP_SIZES[hw.desktop_size ?? ''] ?? hw.desktop_size) : 'Off'} />
        )}
      </SettingsGroup>
      <HardwareDialog hw={editing ? hw : null} close={() => setEditing(false)} />
    </SettingsSection>
  )
}

interface Form {
  cpus: string
  memory: string
  disk: string
  gpu: string
  desktop: boolean
  desktop_size: string
  ports: string[]
  dns: string
  mtu: string
  new_mac: boolean
}

function formOf(hw: Hardware): Form {
  return {
    cpus: String(hw.cpus || ''),
    memory: String(hw.memory_mib / GIB),
    disk: String(hw.disk_gib),
    gpu: hw.gpu,
    desktop: !!hw.desktop,
    desktop_size: hw.desktop_size ?? '',
    ports: hw.ports,
    dns: (hw.dns ?? []).join(', '),
    mtu: hw.mtu ? String(hw.mtu) : '',
    new_mac: false,
  }
}

/** What the form changes against `hw`: only the fields that differ. */
export function changesOf(hw: Hardware, form: Form): HardwareChanges {
  const changes: HardwareChanges = {}
  const cpus = Number(form.cpus)
  if (form.cpus && cpus !== hw.cpus) changes.cpus = cpus
  const memory = Math.round(Number(form.memory) * GIB)
  if (form.memory && memory !== hw.memory_mib) changes.memory_mib = memory
  const disk = Number(form.disk)
  if (form.disk && disk !== hw.disk_gib) changes.disk_gib = disk
  if (form.gpu !== hw.gpu) changes.gpus = form.gpu
  if (hw.kind === 'container') {
    if (form.desktop !== !!hw.desktop) changes.desktop = form.desktop
    if (form.desktop && form.desktop_size !== (hw.desktop_size ?? '')) {
      changes.desktop = true
      changes.desktop_size = form.desktop_size || '1280x800'
    }
    const dns = form.dns.split(/[\s,]+/).filter(Boolean)
    if (dns.join() !== (hw.dns ?? []).join()) changes.dns = dns
    const mtu = form.mtu.trim() ? Number(form.mtu) : 0
    if (mtu !== (hw.mtu ?? 0)) changes.mtu = mtu
  }
  const ports = form.ports.map((p) => p.trim()).filter(Boolean)
  if (ports.join() !== hw.ports.join()) changes.ports = ports
  if (form.new_mac) changes.new_mac = true
  return changes
}

/** A hint under a field: its limit, or what a change does. */
function Hint({ children, warn }: { children: ReactNode; warn?: boolean }) {
  return <p className={`text-caption-1-regular ${warn ? 'text-state-failed-ink' : 'text-ink-2'}`}>{children}</p>
}

/**
 * Changing a container's or VM's hardware, VMware's "Virtual Machine Settings"
 * in one dialog: every change is applied in one re-registration of its VM.
 * A running container restarts to apply it ("Save and restart"); a running VM
 * is shut down from its own system first, so its fields wait until it stops.
 */
export function HardwareDialog({ hw, close }: { hw: Hardware | null; close: () => void }) {
  const [form, setForm] = useState<Form | null>(null)
  const [host, setHost] = useState<DaemonStatus | null>(null)
  // DNS, MTU and the MAC are rarely changed: folded until asked for.
  const [moreOpen, setMoreOpen] = useState(false)
  const router = useRouter()
  // Each opening starts from what is stored.
  const [openedFor, setOpenedFor] = useState<string | null>(null)
  if ((hw?.id ?? null) !== openedFor) {
    setOpenedFor(hw?.id ?? null)
    setForm(hw && formOf(hw))
    setMoreOpen(false)
  }
  useEffect(() => {
    if (hw) daemonStatus().then(setHost, () => setHost(null))
  }, [hw !== null])
  const save = useToastAction(async (changes: HardwareChanges) => {
    if (!hw) return
    if (hw.kind === 'container') await updateContainer({ data: { id: hw.id, changes, restart: hw.running } })
    else await updateMachine({ data: { name: hw.id, changes } })
    close()
    await router.invalidate()
  })
  const shutDown = useToastAction(async () => {
    if (hw) await machineAction({ data: { action: 'stop', name: hw.id } })
    await router.invalidate()
  })
  if (!hw || !form) return null

  const set = <K extends keyof Form>(key: K, value: Form[K]) => setForm({ ...form, [key]: value })
  const container = hw.kind === 'container'
  const macos = hw.os === 'darwin'
  // A full system shuts itself down: nothing changes under it while it runs.
  const locked = !container && hw.running
  const qemuAfter = container ? form.gpu !== '' : hw.backend === 'qemu-hvf'
  const gpus = gpuChoices(hw)
  const changes = changesOf(hw, form)
  const changed = Object.keys(changes).length > 0
  const cores = host?.host_cpus
  const budget = host?.memory_budget_mib
  const memory = Math.round(Number(form.memory) * GIB)
  // What other running VMs hold; this one's own share is free again when it restarts.
  const others = Math.max(0, (host?.assigned_memory_mib ?? 0) - (hw.running ? hw.memory_mib : 0))
  const overBudget = budget !== undefined && memory > budget - others
  const udp = form.ports.some((p) => p.trim().endsWith('/udp'))
  const mtuBad = form.mtu !== '' && !(Number(form.mtu) >= 576 && Number(form.mtu) <= 9000)
  return (
    <FormDialog
      open
      close={close}
      title={`Hardware for ${hw.name}`}
      size="md"
      busy={save.pending}
      submitLabel={container && hw.running ? 'Save and restart' : 'Save'}
      canSubmit={changed && !locked && !(udp && !qemuAfter) && !mtuBad}
      onSubmit={() => save.run(changes)}
    >
      {locked && (
        <Notice
          title={`Shut down ${hw.name} to change its hardware`}
          actions={
            <Button size="sm" variant="secondary" leadingIcon={Power} pending={shutDown.pending} onClick={() => shutDown.run()}>
              Shut down
            </Button>
          }
        >
          Its system gets the power button and turns itself off; most hardware can't change under a running system.
        </Notice>
      )}
      <div className="grid grid-cols-1 gap-4 narrow:grid-cols-3">
        <Field label="Processors">
          <NumberField className="w-full" min={macos ? 2 : 1} max={cores ?? 128} value={form.cpus} disabled={locked} onChange={(cpus) => set('cpus', cpus)} />
          {cores && <Hint>{`Of ${cores} cores`}</Hint>}
        </Field>
        <Field label="Memory">
          <NumberField className="w-full" min={macos ? 4 : 0.25} max={1024} step={0.25} unit="GiB" value={form.memory} disabled={locked} onChange={(m) => set('memory', m)} />
          {budget !== undefined && <Hint warn={overBudget}>{overBudget ? `${memoryText(Math.max(0, budget - others))} free` : `Of ${memoryText(budget)} budget`}</Hint>}
        </Field>
        <Field label="Disk">
          <NumberField className="w-full" min={hw.disk_gib} max={4096} unit="GB" value={form.disk} disabled={locked || macos} onChange={(d) => set('disk', d)} />
          <Hint>{macos ? 'Fixed' : 'Grows only'}</Hint>
        </Field>
      </div>
      {overBudget && <Hint warn>It won't start until other VMs stop and free the VM budget.</Hint>}

      {!macos && (
        <SettingsGroup>
          <SettingsRow
            icon={Zap}
            label="GPU"
            hint={gpus ? (container ? `Runs on ${qemuAfter ? 'QEMU/HVF' : 'Apple Virtualization'}` : undefined) : 'Apple Virtualization has none'}
            control={
              gpus ? (
                <Select items={gpus} value={form.gpu} disabled={locked} onValueChange={(gpu) => set('gpu', gpu ?? '')}>
                  <SelectTrigger className="w-52">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {Object.entries(gpus).map(([value, label]) => (
                      <SelectItem key={value} value={value}>
                        {label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              ) : undefined
            }
          />
          {container && (
            <SettingsRow
              icon={Monitor}
              label="Display"
              // Apple Virtualization has one fixed 2D scanout (docs/known-issues.md).
              hint={form.desktop && !qemuAfter ? 'Fixed at 1280×800 without a GPU; choose a GPU to pick its size' : undefined}
              control={
                <span className="flex items-center gap-3">
                  {form.desktop && !qemuAfter && (
                    <Select items={{ fixed: '1280×800' }} value="fixed" disabled>
                      <SelectTrigger className="w-32">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="fixed">1280×800</SelectItem>
                      </SelectContent>
                    </Select>
                  )}
                  {form.desktop && qemuAfter && (
                    <Select items={DESKTOP_SIZES} value={form.desktop_size} onValueChange={(v) => set('desktop_size', v ?? '')}>
                      <SelectTrigger className="w-32">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {Object.entries(DESKTOP_SIZES).map(([value, label]) => (
                          <SelectItem key={value} value={value}>
                            {label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  )}
                  <Switch aria-label="Display" checked={form.desktop} onCheckedChange={(desktop) => set('desktop', desktop)} />
                </span>
              }
            />
          )}
        </SettingsGroup>
      )}

      {!macos && (container || hw.backend === 'qemu-hvf') && (
        <SettingsSection
          title={
            <span className="inline-flex items-center gap-1">
              Ports
              <InfoTip label="About Ports">{`The network is NAT, sharing this Mac's connection. HOST:GUEST${container ? '[/udp]' : ''}, on 127.0.0.1 of this Mac.`}</InfoTip>
            </span>
          }
          actions={
            <Button size="sm" variant="secondary" leadingIcon={Plus} disabled={locked} onClick={() => set('ports', [...form.ports, ''])}>
              Add
            </Button>
          }
        >
          <PortList ports={form.ports} disabled={locked} onChange={(ports) => set('ports', ports)} />
          {udp && !qemuAfter && <Hint warn>UDP needs QEMU/HVF: pick a GPU or publish TCP only.</Hint>}
        </SettingsSection>
      )}
      {!macos && (
        <SettingsSection
          title={
            <span className="inline-flex items-center gap-1">
              Network options
              <InfoTip label="About network options">{container ? 'DNS servers, MTU and the MAC address.' : 'The MAC address.'}</InfoTip>
            </span>
          }
          actions={
            <Button size="sm" variant="secondary" onClick={() => setMoreOpen(!moreOpen)}>
              {moreOpen ? 'Hide' : 'Show'}
            </Button>
          }
        >
          {moreOpen && (
          <div className="flex flex-col gap-4">
            {container && (
              <div className="grid grid-cols-1 gap-4 narrow:grid-cols-2">
                <Field label="DNS servers">
                  <Input className="font-mono" placeholder="Default" value={form.dns} onChange={(e) => set('dns', e.target.value)} />
                </Field>
                <Field label="MTU">
                  <Input inputMode="numeric" placeholder="Default" value={form.mtu} onChange={(e) => set('mtu', e.target.value.replace(/\D/g, ''))} />
                  {mtuBad && <Hint warn>576 to 9000.</Hint>}
                </Field>
              </div>
            )}
            <SettingsGroup>
              <SettingsRow
                icon={Network}
                label="MAC address"
                hint="A new address is a new network card to the guest."
                value={<span className={form.new_mac ? undefined : 'font-mono'}>{form.new_mac ? 'A new one on save' : (hw.mac ?? 'Assigned by QEMU')}</span>}
                actions={
                  <Button size="sm" variant="secondary" disabled={locked} onClick={() => set('new_mac', !form.new_mac)}>
                    {form.new_mac ? 'Keep' : 'New'}
                  </Button>
                }
              />
            </SettingsGroup>
          </div>
          )}
        </SettingsSection>
      )}
    </FormDialog>
  )
}

/**
 * Published ports, one row each in one group: the forward typed in place
 * (`HOST:GUEST[/udp]`) and its remove button; the section's Add adds a row.
 */
function PortList({ ports, disabled, onChange }: { ports: string[]; disabled?: boolean; onChange: (ports: string[]) => void }) {
  return (
    <SettingsGroup>
      {ports.length === 0 && <div className="flex h-12 items-center text-body-regular text-ink-2">None</div>}
      {ports.map((p, i) => (
        <div key={i} className="flex h-12 items-center gap-2">
          <input
            className="min-w-0 flex-1 bg-transparent font-mono text-body-regular text-ink outline-none placeholder:text-ink-3 disabled:opacity-45"
            aria-label={`Port ${i + 1}`}
            placeholder="8080:80"
            // A row just added is typed into at once.
            autoFocus={p === '' && i === ports.length - 1}
            value={p}
            disabled={disabled}
            onChange={(e) => onChange(ports.map((q, j) => (j === i ? e.target.value : q)))}
          />
          <IconButton variant="ghost" size="sm" icon={X} aria-label={`Remove port ${i + 1}`} disabled={disabled} onClick={() => onChange(ports.filter((_, j) => j !== i))} />
        </div>
      ))}
    </SettingsGroup>
  )
}
