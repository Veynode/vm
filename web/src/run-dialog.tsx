import { useState } from 'react'
import { NumberField } from '@console/components/ui/NumberField'
import { Input } from '@ui/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@ui/components/ui/select'
import { Switch } from '@ui/components/ui/switch'
import { Textarea } from '@ui/components/ui/textarea'
import { Monitor, Server, Zap } from '@ui/icons'
import { Field } from '@ui/settings/field'
import { InfoTip } from '@ui/settings/info-tip'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup } from '@ui/settings/settings-section'
import { runContainer } from './api'
import { FormDialog, lines, useToastAction, words } from './ui'
import { useGo } from './page-nav'

export const DESKTOP_SIZES: Record<string, string> = {
  '': '1280×800',
  '1920x1080': '1920×1080',
}

export const GPUS: Record<string, string> = {
  '': 'None',
  virgl: 'virgl — OpenGL',
  venus: 'Venus — Vulkan (experimental)',
}

const EMPTY = {
  image: '',
  command: '',
  name: '',
  ports: '',
  volumes: '',
  env: '',
  // The CLI's own default (`--cpus`), shown so the steppers start from it.
  cpus: '2',
  memory: '',
  gpus: '',
  desktop: false,
  desktop_size: '',
  shared: false,
}

/**
 * Running a container: a dialog over Containers or Images (creating opens a
 * sheet). `image` fills the Image field when it opens from an image's row.
 * Run opens the new container's page; a failed run is a toast and the form
 * keeps what was typed.
 */
export function RunDialog({ open, close, image }: { open: boolean; close: () => void; image?: string }) {
  const go = useGo()
  const [form, setForm] = useState(EMPTY)
  // Each opening starts from the image it was opened for.
  const [openedFor, setOpenedFor] = useState<string | undefined | null>(null)
  if (open && openedFor !== (image ?? '')) {
    setOpenedFor(image ?? '')
    setForm({ ...EMPTY, image: image ?? '' })
  }
  if (!open && openedFor !== null) setOpenedFor(null)
  const set = (key: keyof typeof form) => (e: { target: { value: string } }) => setForm({ ...form, [key]: e.target.value })
  const run = useToastAction(async () => {
    const result = await runContainer({
      data: {
        image: form.image.trim(),
        command: words(form.command),
        name: form.name.trim() || undefined,
        ports: lines(form.ports.replace(/,/g, '\n')),
        volumes: lines(form.volumes),
        env: lines(form.env),
        cpus: form.cpus ? Number(form.cpus) : undefined,
        memory: form.memory.trim() || undefined,
        gpus: form.gpus || undefined,
        desktop: form.desktop,
        desktop_size: form.desktop && form.gpus ? form.desktop_size || undefined : undefined,
        shared: form.shared,
      },
    })
    close()
    if (form.shared) go({ to: '/about' })
    else go({ to: '/containers/$id', params: { id: result.id } })
  })
  const backend = form.shared ? 'Shared engine' : form.gpus ? 'QEMU/HVF micro-VM' : 'Apple Virtualization micro-VM'
  return (
    <FormDialog
      open={open}
      close={close}
      title="Run a container"
      size="md"
      busy={run.pending}
      submitLabel="Run"
      canSubmit={!!form.image.trim()}
      onSubmit={() => run.run()}
    >
      <Field label="Image">
        <Input className="font-mono" required placeholder="nginx:alpine" value={form.image} onChange={set('image')} autoFocus />
      </Field>
      <Field label="Command" trailingAction={<InfoTip label="About Command">Optional. Overrides the image's Cmd.</InfoTip>}>
        <Input className="font-mono" placeholder={`sh -c 'echo hello'`} value={form.command} onChange={set('command')} />
      </Field>
      <Field label="Name">
        <Input value={form.name} placeholder="Chosen for you" onChange={set('name')} />
      </Field>
      <Field label="Ports" trailingAction={<InfoTip label="About Ports">HOST:GUEST, comma separated.</InfoTip>}>
        <Input className="font-mono" placeholder="8080:80" value={form.ports} onChange={set('ports')} />
      </Field>
      <div className="grid grid-cols-1 gap-4 narrow:grid-cols-2">
        <Field label="CPUs">
          <NumberField className="w-full" min={1} max={32} value={form.cpus} onChange={(cpus) => setForm({ ...form, cpus })} />
        </Field>
        <Field label="Memory" trailingAction={<InfoTip label="About Memory">512m by default, 2g with a GPU.</InfoTip>}>
          <Input placeholder="512m" value={form.memory} onChange={set('memory')} />
        </Field>
      </div>
      <Field label="GPU" trailingAction={<InfoTip label="About GPU">A GPU runs the container on QEMU/HVF.</InfoTip>}>
        <Select items={GPUS} value={form.gpus} onValueChange={(gpus) => setForm({ ...form, gpus: gpus ?? '' })}>
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {Object.entries(GPUS).map(([value, label]) => (
              <SelectItem key={value} value={value}>
                {label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Field>
      <Field label="Volumes" trailingAction={<InfoTip label="About Volumes">One per line: /host/path:/container/path[:ro]</InfoTip>}>
        <Textarea className="font-mono" rows={2} value={form.volumes} onChange={set('volumes')} />
      </Field>
      <Field label="Environment" trailingAction={<InfoTip label="About Environment">One KEY=VALUE per line.</InfoTip>}>
        <Textarea className="font-mono" rows={3} value={form.env} onChange={set('env')} />
      </Field>
      <SettingsGroup>
        <SettingsRow icon={Monitor} label="Desktop display" control={<Switch checked={form.desktop} onCheckedChange={(desktop) => setForm({ ...form, desktop })} />} />
        {form.desktop && form.gpus ? (
          <SettingsRow
            icon={Monitor}
            label="Display size"
            control={
              <Select items={DESKTOP_SIZES} value={form.desktop_size} onValueChange={(desktop_size) => setForm({ ...form, desktop_size: desktop_size ?? '' })}>
                <SelectTrigger>
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
            }
          />
        ) : null}
        <SettingsRow icon={Server} label="In the shared engine" control={<Switch checked={form.shared} onCheckedChange={(shared) => setForm({ ...form, shared })} />} />
        <SettingsRow icon={Zap} label="Runs on" value={backend} />
      </SettingsGroup>
    </FormDialog>
  )
}
