import { useEffect, useState } from 'react'
import { NumberField } from '@console/components/ui/NumberField'
import { Input } from '@ui/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@ui/components/ui/select'
import { Switch } from '@ui/components/ui/switch'
import { Textarea } from '@ui/components/ui/textarea'
import { SquareTerminal, Zap } from '@ui/icons'
import { Field } from '@ui/settings/field'
import { InfoTip } from '@ui/settings/info-tip'
import { Notice } from '@ui/settings/notice'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup } from '@ui/settings/settings-section'
import { createMachine, macosImageStatus, runContainer, startMacosPull } from './api'
import type { PullState } from './server/pulls'
import { ConfirmDialog, FormDialog, Hint, gibToMemory, lines, message, size, useToast, useToastAction } from './ui'
import { useGo } from './page-nav'

type System = 'linux' | 'windows' | 'macos'

const SYSTEMS: Record<System, string> = {
  linux: 'Linux distribution',
  windows: 'Windows 11 ARM64',
  macos: 'macOS',
}

const GPUS: Record<'linux' | 'windows', Record<string, string>> = {
  linux: { '': 'None', virgl: 'virgl — OpenGL', venus: 'Venus — Vulkan (experimental)' },
  windows: { '': 'None — Basic Display, ~32 fps', neptune: 'Neptune — DirectX, 60 fps (experimental, viogpu3d)' },
}

const BACKENDS: Record<string, string> = { apple: 'Apple Virtualization', qemu: 'QEMU/HVF' }
const DISK_BUSES: Record<string, string> = { virtio: 'virtio-blk', nvme: 'NVMe' }

const MACOS_IMAGES: Record<string, string> = {
  'ghcr.io/cirruslabs/macos-tahoe-vanilla': 'macOS 26 Tahoe',
  'ghcr.io/cirruslabs/macos-sequoia-vanilla': 'macOS 15 Sequoia',
}
/** README: ~22 GB download, ~26 GB on disk; the pull stops below 8 GiB free (crates/oci/src/tart.rs RESERVE). */
const MACOS_ON_DISK = 26 * 2 ** 30
const PULL_RESERVE = 8 * 2 ** 30

/** `machine create`'s own defaults; a macOS container takes its image's (empty). */
const SIZES: Record<System, { cpus: string; memory: string; diskSize: string }> = {
  linux: { cpus: '4', memory: '4', diskSize: '32' },
  windows: { cpus: '4', memory: '4', diskSize: '64' },
  macos: { cpus: '', memory: '', diskSize: '' },
}

const EMPTY = {
  ...SIZES.linux,
  system: 'linux' as System,
  name: '',
  iso: '',
  virtio: '',
  attach: '',
  gpus: '',
  backend: 'apple',
  diskBus: 'virtio',
  monitor: false,
  ports: '',
  image: 'ghcr.io/cirruslabs/macos-tahoe-vanilla',
}

type MacStatus = { pulled: boolean; free_bytes: number; pull: PullState | null }

function Choice({ items, value, onChange, disabled }: { items: Record<string, string>; value: string; onChange: (v: string) => void; disabled?: boolean }) {
  return (
    <Select items={items} value={value} disabled={disabled} onValueChange={(v) => onChange(v ?? '')}>
      <SelectTrigger>
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {Object.entries(items).map(([v, label]) => (
          <SelectItem key={v} value={v}>
            {label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

/**
 * A new VM: a Linux distribution or Windows 11 ARM64 installed from the
 * user's own ISO (`veynodevm machine create`), or a macOS container from a
 * Tart image (`veynodevm run`), pulled first after the 22 GB is confirmed.
 * Create opens the new VM's page; a failure is a toast with the CLI's error
 * and the form keeps what was typed.
 */
export function NewMachineDialog({ open, close }: { open: boolean; close: () => void }) {
  const go = useGo()
  const toast = useToast()
  const [form, setForm] = useState(EMPTY)
  const [wasOpen, setWasOpen] = useState(false)
  if (open !== wasOpen) {
    setWasOpen(open)
    if (open) setForm(EMPTY)
  }
  const set = (key: keyof typeof form) => (e: { target: { value: string } }) => setForm({ ...form, [key]: e.target.value })
  const choose = (key: keyof typeof form) => (value: string) => setForm({ ...form, [key]: value })
  // A size still at the old system's default takes the new one's; one the user set stays.
  const switchSystem = (next: System) => {
    const sizes = { ...SIZES[next] }
    for (const key of ['cpus', 'memory', 'diskSize'] as const) if (form[key] !== SIZES[form.system][key]) sizes[key] = form[key]
    setForm({ ...form, ...sizes, system: next, gpus: '' })
  }

  const system = form.system
  const qemu = system === 'windows' || !!form.gpus || form.backend === 'qemu'

  // macOS: whether the image is here, the free space, and a running pull.
  const [mac, setMac] = useState<MacStatus | null>(null)
  const [confirming, setConfirming] = useState(false)
  const pulling = mac?.pull?.state === 'pulling'
  useEffect(() => {
    if (!open || system !== 'macos') return
    let live = true
    const load = () =>
      macosImageStatus({ data: form.image })
        .then((status) => {
          if (!live) return
          setMac((previous) => {
            if (previous?.pull?.state === 'pulling' && status.pull?.state === 'done') toast(`Downloaded ${form.image}`)
            if (previous?.pull?.state === 'pulling' && status.pull?.state === 'failed') toast(status.pull.error ?? 'Pull failed', 'error')
            return status
          })
        })
        .catch((e) => live && toast(message(e), 'error'))
    void load()
    const timer = pulling ? setInterval(load, 1000) : undefined
    return () => {
      live = false
      if (timer) clearInterval(timer)
    }
  }, [open, system, form.image, pulling, toast])

  const pull = useToastAction(async () => {
    const state = await startMacosPull({ data: form.image })
    setMac((m) => (m ? { ...m, pull: state } : m))
    setConfirming(false)
  })

  const create = useToastAction(async () => {
    if (system === 'macos') {
      const result = await runContainer({
        data: {
          image: form.image,
          command: [],
          name: form.name.trim() || undefined,
          ports: [],
          volumes: [],
          env: [],
          cpus: form.cpus ? Number(form.cpus) : undefined,
          memory: gibToMemory(form.memory),
          disk_size: form.diskSize ? Number(form.diskSize) : undefined,
        },
      })
      close()
      go({ to: '/containers/$id', params: { id: result.id } })
      return
    }
    const attach = system === 'windows' ? [form.virtio.trim(), ...lines(form.attach)].filter(Boolean) : lines(form.attach)
    const result = await createMachine({
      data: {
        os: system,
        name: form.name.trim(),
        iso: form.iso.trim() || undefined,
        diskSize: form.diskSize.trim() ? `${form.diskSize}G` : undefined,
        memory: gibToMemory(form.memory),
        cpus: form.cpus ? Number(form.cpus) : undefined,
        gpus: (form.gpus || undefined) as 'virgl' | 'venus' | 'neptune' | undefined,
        backend: system === 'linux' && !form.gpus ? (form.backend as 'apple' | 'qemu') : undefined,
        diskBus: qemu && form.diskBus === 'nvme' ? 'nvme' : undefined,
        monitor: qemu && form.monitor,
        ports: qemu ? lines(form.ports.replace(/,/g, '\n')) : [],
        attach,
      },
    })
    close()
    go({ to: '/machines/$id', params: { id: result.id } })
  })

  const needsPull = system === 'macos' && mac !== null && !mac.pulled
  const lowSpace = mac !== null && mac.free_bytes < MACOS_ON_DISK + PULL_RESERVE
  const canSubmit = system === 'macos' ? mac !== null && !pulling : !!form.name.trim() && (system !== 'windows' || !!form.iso.trim())
  const progress = mac?.pull
  return (
    <>
      <FormDialog
        open={open}
        close={close}
        title="New VM"
        size="md"
        busy={create.pending}
        submitLabel={pulling ? 'Downloading…' : needsPull ? 'Download…' : 'Create'}
        canSubmit={canSubmit}
        onSubmit={() => (needsPull ? setConfirming(true) : create.run())}
      >
        <Field label="System">
          <Choice items={SYSTEMS} value={system} onChange={(v) => switchSystem(v as System)} />
        </Field>
        {system === 'windows' && (
          <Notice title="Bring your own Windows 11 ARM64 ISO and the virtio-win driver ISO">
            Neither is downloaded for you. Setup needs the virtio-win drivers to see the disk and network (or choose the NVMe disk bus).
            For an unattended install, scripts/install-distro.py windows writes the Autounattend image to attach.
          </Notice>
        )}
        {system === 'macos' ? (
          <>
            <Field label="Image">
              <Choice items={MACOS_IMAGES} value={form.image} disabled={pulling} onChange={(image) => setForm({ ...form, image })} />
            </Field>
            {mac && !mac.pulled && !progress && (
              <Notice status="waiting" title="Not downloaded yet">
                {`About 22 GB to download, about 26 GB on disk. ${size(mac.free_bytes)} free.`}
              </Notice>
            )}
            {mac && !mac.pulled && lowSpace && (
              <Notice status="error" title="Low disk space">
                {`Only ${size(mac.free_bytes)} free. The download stops when free space falls below 8 GiB; finished chunks resume next time.`}
              </Notice>
            )}
            {progress && progress.state !== 'done' && <PullProgress pull={progress} free={mac?.free_bytes} />}
          </>
        ) : (
          <>
            <Field label="Name">
              <Input required value={form.name} placeholder={system === 'windows' ? 'win11' : 'ubuntu'} onChange={set('name')} autoFocus />
            </Field>
            <Field
              label={system === 'windows' ? 'Windows 11 ARM64 ISO' : 'Installer ISO'}
              trailingAction={<InfoTip label="About the ISO">A path on this Mac. Attached read-only until you eject it.</InfoTip>}
            >
              <Input className="font-mono" placeholder="/Users/you/Downloads/installer.iso" value={form.iso} onChange={set('iso')} />
            </Field>
            {system === 'windows' && (
              <Field label="virtio-win ISO" trailingAction={<InfoTip label="About virtio-win">The Fedora virtio-win driver ISO, attached read-only.</InfoTip>}>
                <Input className="font-mono" placeholder="/Users/you/Downloads/virtio-win.iso" value={form.virtio} onChange={set('virtio')} />
              </Field>
            )}
            <Field
              label={system === 'windows' ? 'More images' : 'Seed images'}
              trailingAction={<InfoTip label="About attached images">One path per line (.iso, .img, .raw), attached read-only: cloud-init, kickstart or Autounattend.</InfoTip>}
            >
              <Textarea className="font-mono" rows={2} placeholder="/Users/you/Downloads/seed.iso" value={form.attach} onChange={set('attach')} />
            </Field>
          </>
        )}
        {system === 'macos' && (
          <Field label="Name">
            <Input value={form.name} placeholder="Chosen for you" onChange={set('name')} />
          </Field>
        )}
        <div className="grid grid-cols-1 gap-4 narrow:grid-cols-3">
          <Field label="CPUs">
            <NumberField className="w-full" min={1} max={32} value={form.cpus} onChange={(cpus) => setForm({ ...form, cpus })} />
            {system === 'macos' && !form.cpus && <Hint>From the image</Hint>}
          </Field>
          <Field label="Memory">
            <NumberField className="w-full" min={0.5} max={128} step={0.5} unit="GiB" value={form.memory} onChange={(memory) => setForm({ ...form, memory })} />
            {system === 'macos' && !form.memory && <Hint>From the image</Hint>}
          </Field>
          <Field label="Disk" trailingAction={<InfoTip label="About Disk">{system === 'macos' ? 'Grows the image disk.' : 'Sparse: takes host space as the guest writes.'}</InfoTip>}>
            <NumberField className="w-full" min={4} max={4096} unit="GiB" value={form.diskSize} onChange={(diskSize) => setForm({ ...form, diskSize })} />
            {system === 'macos' && !form.diskSize && <Hint>From the image</Hint>}
          </Field>
        </div>
        {system !== 'macos' && (
          <>
            <Field label="GPU" trailingAction={<InfoTip label="About GPU">A GPU runs the VM on QEMU/HVF.</InfoTip>}>
              <Choice items={GPUS[system]} value={form.gpus} onChange={choose('gpus')} />
            </Field>
            {system === 'linux' && !form.gpus && (
              <Field label="Backend">
                <Choice items={BACKENDS} value={form.backend} onChange={choose('backend')} />
              </Field>
            )}
            {qemu && (
              <>
                <Field label="Ports" trailingAction={<InfoTip label="About Ports">HOST:GUEST TCP, comma separated; 127.0.0.1 on this Mac.</InfoTip>}>
                  <Input className="font-mono" placeholder={system === 'windows' ? '2222:22, 3389:3389' : '2222:22'} value={form.ports} onChange={set('ports')} />
                </Field>
                <Field label="Disk bus" trailingAction={<InfoTip label="About Disk bus">NVMe needs no driver during Windows Setup.</InfoTip>}>
                  <Choice items={DISK_BUSES} value={form.diskBus} onChange={choose('diskBus')} />
                </Field>
              </>
            )}
            <SettingsGroup>
              {qemu && <SettingsRow icon={SquareTerminal} label="QMP monitor" control={<Switch checked={form.monitor} onCheckedChange={(monitor) => setForm({ ...form, monitor })} />} />}
              <SettingsRow icon={Zap} label="Runs on" value={qemu ? 'QEMU/HVF, UEFI' : 'Apple Virtualization, UEFI'} />
            </SettingsGroup>
          </>
        )}
      </FormDialog>
      <ConfirmDialog
        open={confirming}
        title={`Download ${MACOS_IMAGES[form.image] ?? form.image}?`}
        confirmLabel="Download"
        confirmVariant="default"
        busy={pull.pending}
        onConfirm={() => pull.run()}
        close={() => setConfirming(false)}
      >
        {`About 22 GB from ghcr.io, about 26 GB on disk; ${mac ? size(mac.free_bytes) : 'unknown'} free now. It keeps going if you close this dialog, and an interrupted download resumes.`}
      </ConfirmDialog>
    </>
  )
}

function PullProgress({ pull, free }: { pull: PullState; free?: number }) {
  const ratio = pull.total ? (pull.done ?? 0) / pull.total : 0
  const failed = pull.state === 'failed'
  return (
    <div className="flex flex-col gap-2">
      {failed ? (
        <Notice status="error" title="Download failed">
          {pull.error}
        </Notice>
      ) : (
        <>
          <div className="flex items-center justify-between text-body-regular text-ink-2">
            <span>{pull.total ? `Downloading: ${pull.done ?? 0} of ${pull.total} disk chunks` : 'Resolving the image'}</span>
            {free !== undefined && <span>{`${size(free)} free`}</span>}
          </div>
          <div role="progressbar" aria-label="Download" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(ratio * 100)} className="h-1.5 overflow-hidden rounded-full bg-surface-2">
            <div className="h-full rounded-full bg-ink transition-[width]" style={{ width: `${ratio * 100}%` }} />
          </div>
        </>
      )}
      {!failed && pull.lines.length > 0 && <p className="truncate font-mono text-caption-1-regular text-ink-2">{pull.lines.at(-1)}</p>}
    </div>
  )
}
