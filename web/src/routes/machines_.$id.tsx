import { useMemo, useState } from 'react'
import { createFileRoute, useRouter } from '@tanstack/react-router'
import { Button } from '@ui/components/ui/button'
import { Input } from '@ui/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@ui/components/ui/select'
import { AppWindow, ExternalLink, Calendar, CircleDot, Disc, Disc3, FileText, HardDrive, KeyRound, Monitor, Palette, PenLine, Power, SquareTerminal, Terminal, Trash2, User } from '@ui/icons'
import { Notice } from '@ui/settings/notice'
import { RowIcon } from '@ui/settings/list-row'
import { RowMenu } from '@ui/settings/row-menu'
import { SettingsLinkRow } from '@ui/settings/settings-link-row'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup, SettingsSection } from '@ui/settings/settings-section'
import { inspectMachine, machineAction, openDesktop, setMachineDisplay, setMachineSshKey, sshForward } from '../api'
import type { MachineAction } from '../machine-input.mjs'
import { ConfirmDialog, PageActions, PageBody, PanelLoadError, StatusTag, age, message, size, usePoll, useSubPage, useToast, useToastAction } from '../ui'
import { MachineIcon } from '../container-icon'
import { MachineIconDialog } from '../icon-dialog'
import { RenameMachineDialog } from '../rename-dialog'
import { HardwareSection, machineHardware } from '../hardware-dialog'
import { machineUp, osLabel } from './machines'
import { useGo } from '../page-nav'
import { DesktopDialog, TerminalDialog } from '../console-dialog'

export const Route = createFileRoute('/machines_/$id')({
  loader: ({ params }) => inspectMachine({ data: params.id }),
  component: Detail,
  errorComponent: ({ error, reset }) => <PanelLoadError title="Couldn't load this VM." detail={message(error)} onRetry={reset} />,
})

function Value({ children, mono }: { children: string; mono?: boolean }) {
  return (
    <span title={children} className={mono ? 'font-mono' : undefined}>
      {children}
    </span>
  )
}

/**
 * A machine's own page under VMs: its state with Start/Shut down, its
 * terminal (over SSH) and desktop (where the browser desktop covers its
 * backend) and native window as rows, the rest in its ··· menu;
 * then what `machine inspect` reports and, for Windows, the guest login MCP
 * uses.
 */
function Detail() {
  const { machine: m, vm, ssh } = Route.useLoaderData()
  const router = useRouter()
  const go = useGo()
  const toast = useToast()
  usePoll(2000)
  const state: string = vm?.state ?? 'missing'
  const up = machineUp(state)
  // Its name after its icon, as a container's page; the same node between polls.
  const heading = useMemo(
    () => (
      <span className="flex min-w-0 items-center gap-2.5">
        <MachineIcon icon={m.icon} up={up} />
        <span className="truncate">{m.name}</span>
      </span>
    ),
    [m.icon, m.name, up],
  )
  useSubPage(heading, () => go({ to: '/machines' }))
  const config = vm?.config ?? {}
  const qemu = m.backend === 'qemu-hvf'
  const action = useToastAction(async (act: MachineAction) => {
    await machineAction({ data: { action: act, name: m.name } })
    if (act === 'rm') go({ to: '/machines' })
    else {
      if (act === 'eject') toast(`Ejected the installer from ${m.name}`)
      await router.invalidate()
    }
  })
  const nativeWindow = useToastAction(async () => {
    await openDesktop({ data: m.name })
  })
  const [removing, setRemoving] = useState(false)
  const [killing, setKilling] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [changingIcon, setChangingIcon] = useState(false)
  const [opened, setOpened] = useState<'terminal' | 'desktop' | null>(null)

  // The browser desktop streams QEMU's console broker; Apple Virtualization
  // needs the guest agent, which installed distributions do not have.
  const browserDesktop = up && qemu && !!vm?.runtime?.console_socket
  const nativeDesktop = up && (!qemu || vm?.capabilities?.native_window === true)
  // An SSH login through the forward to guest port 22 (`machine::terminal`).
  const terminal = up && !!(ssh?.user && ssh.key) && sshForward(config)
  const extra: { path: string }[] = config.extra_disks ?? []
  const attached = !!config.cdrom || extra.length > 0
  return (
    <PageBody label={m.name}>
      <PageActions>
        <RowMenu
          label={`More for ${m.name}`}
          items={[
            { label: 'Rename', icon: PenLine, disabled: up, onSelect: () => setRenaming(true) },
            { label: 'Change icon', icon: Palette, onSelect: () => setChangingIcon(true) },
            up && { label: 'Force stop', icon: Power, onSelect: () => setKilling(true) },
            attached && { label: 'Eject installer', icon: Disc, disabled: up, onSelect: () => action.run('eject') },
            { label: 'Remove', icon: Trash2, danger: true, apart: true, disabled: up, onSelect: () => setRemoving(true) },
          ]}
        />
      </PageActions>
      {!vm && <Notice status="error">The daemon has no VM registered for this machine, or it is not running.</Notice>}
      {vm?.invalid_reason && <Notice status="error">{String(vm.invalid_reason)}</Notice>}
      {attached && !up && m.iso && (
        <Notice title="The installer is still attached">Once the system is installed, eject it so the VM boots from its disk.</Notice>
      )}
      <SettingsGroup>
        <SettingsRow
          icon={CircleDot}
          label="State"
          state={<StatusTag state={state} status={state} />}
          value={vm?.exit_code != null ? `exit ${vm.exit_code}` : undefined}
          actions={
            <Button size="sm" variant="secondary" title={up ? 'ACPI power button: the system shuts itself down' : undefined} pending={action.pending} disabled={state === 'missing'} onClick={() => action.run(up ? 'stop' : 'start')}>
              {up ? 'Shut down' : 'Start'}
            </Button>
          }
        />
        {terminal && (
          <SettingsLinkRow icon={<RowIcon icon={Terminal} />} label="Terminal" meta={`SSH as ${ssh?.user}`} onOpen={() => setOpened('terminal')} />
        )}
        {browserDesktop && (
          <SettingsLinkRow icon={<RowIcon icon={Monitor} />} label="Desktop" meta="In this browser" onOpen={() => setOpened('desktop')} />
        )}
        {up && (
          // Opens a window on this Mac; the window itself closes it.
          <SettingsLinkRow
            icon={<RowIcon icon={AppWindow} />}
            label="Native window"
            meta={nativeDesktop ? 'On this Mac' : 'Needs the QEMU console app'}
            value={<ExternalLink className="size-4 text-ink-2" aria-hidden />}
            chevron={false}
            disabled={nativeWindow.pending || !nativeDesktop}
            onOpen={() => nativeWindow.run()}
          />
        )}
      </SettingsGroup>
      <SettingsGroup>
        <SettingsRow icon={m.os === 'windows' ? AppWindow : SquareTerminal} label="System" value={osLabel(m.os)} />
        {qemu && <SettingsRow icon={HardDrive} label="Disk bus" value={config.qemu_disk_bus === 'nvme' ? 'NVMe' : 'virtio-blk'} />}
        <SettingsRow icon={Disc3} label="Installer ISO" value={<Value mono>{config.cdrom ?? m.iso ?? '—'}</Value>} />
        <SettingsRow icon={Disc} label="Attached images" value={<Value mono>{extra.map((d) => d.path).join(', ') || '—'}</Value>} />
        {qemu && <SettingsRow icon={SquareTerminal} label="QMP monitor" value={config.qemu_monitor ? 'On' : 'Off'} />}
        <SettingsRow icon={FileText} label="Serial log" value={<Value mono>{config.serial_log ?? '—'}</Value>} />
        <SettingsRow icon={Calendar} label="Created" value={age(m.created_at)} />
      </SettingsGroup>
      {vm && <HardwareSection hw={machineHardware(m, config, up)} />}
      {m.os === 'windows' && up && !!ssh?.user && ssh.key && <DisplaySize name={m.name} />}
      {m.os === 'windows' && <SshLogin name={m.name} ssh={ssh} />}
      <TerminalDialog target={opened === 'terminal' ? { id: m.id, name: m.name } : null} close={() => setOpened(null)} />
      <DesktopDialog target={opened === 'desktop' ? { id: m.id, name: m.name } : null} close={() => setOpened(null)} />
      <MachineIconDialog machine={changingIcon ? m : null} close={() => setChangingIcon(false)} />
      {/* The page's address is the name, so it moves to the new one. */}
      <RenameMachineDialog
        machine={renaming ? m : null}
        close={() => setRenaming(false)}
        renamed={() => router.invalidate()}
      />
      <ConfirmDialog
        open={killing}
        title={`Force stop ${m.name}?`}
        confirmLabel="Force stop"
        busy={action.pending}
        onConfirm={async () => {
          await action.run('kill')
          setKilling(false)
        }}
        close={() => setKilling(false)}
      >
        The VM is killed like pulling its power cord; anything the guest has not written to disk is lost.
      </ConfirmDialog>
      <ConfirmDialog
        open={removing}
        title={`Remove ${m.name}?`}
        confirmLabel="Remove"
        busy={action.pending}
        onConfirm={async () => {
          await action.run('rm')
          setRemoving(false)
        }}
        close={() => setRemoving(false)}
      >
        {`Its ${size(m.disk_gib * 2 ** 30)} disk, UEFI variables and serial log are deleted. The installer ISO and attached images on this Mac are left alone.`}
      </ConfirmDialog>
    </PageBody>
  )
}

/**
 * `machine display`: the guest's resolution, set through its SSH login (Windows
 * only lets the logged-in desktop change it). Windows keeps the choice across
 * restarts while the machine's device arguments stay the same.
 */
function DisplaySize({ name }: { name: string }) {
  const toast = useToast()
  // What the guest reports is not read back, so it shows the last one chosen here.
  const [size, setSize] = useState<string | null>(null)
  const apply = useToastAction(async (next: string) => {
    await setMachineDisplay({ data: { name, size: next } })
    setSize(next)
    toast(`Set ${name} to ${RESOLUTIONS[next]}`)
  })
  return (
    <SettingsSection title="Display" description="The guest must offer the mode (Neptune offers up to 2048×1152). The browser desktop streams up to 1920×1080.">
      <SettingsGroup>
        <SettingsRow
          icon={Monitor}
          label="Resolution"
          control={
            <Select items={RESOLUTIONS} value={size} disabled={apply.pending} onValueChange={(next) => next && next !== size && apply.run(next)}>
              <SelectTrigger className="w-40">
                <SelectValue placeholder="Choose" />
              </SelectTrigger>
              <SelectContent>
                {Object.entries(RESOLUTIONS).map(([value, label]) => (
                  <SelectItem key={value} value={value}>
                    {label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          }
        />
      </SettingsGroup>
    </SettingsSection>
  )
}

/** Common modes within `machine display`'s 640x480 to 3840x2160. */
const RESOLUTIONS: Record<string, string> = Object.fromEntries(
  ['1024x768', '1280x720', '1280x800', '1366x768', '1440x900', '1600x900', '1920x1080', '1920x1200', '2048x1152', '2560x1440', '3840x2160'].map((s) => [s, s.replace('x', '×')]),
)

/**
 * `machine ssh-key`: the Windows account and private key MCP's vm_exec, files
 * and desktop tools log in with. Only the key's path leaves the browser; the
 * CLI copies the file on the host.
 */
function SshLogin({ name, ssh }: { name: string; ssh: { user: string | null; key: boolean } | null }) {
  const router = useRouter()
  const toast = useToast()
  const [user, setUser] = useState('')
  const [key, setKey] = useState('')
  const save = useToastAction(async () => {
    await setMachineSshKey({ data: { name, user: user.trim(), key: key.trim() } })
    setKey('')
    toast('Saved the guest login')
    await router.invalidate()
  })
  return (
    <SettingsSection title="Guest login" description="The Windows account and OpenSSH private key MCP uses for vm_exec, files and the desktop tools: the key the answer file authorized.">
      <SettingsGroup>
        <SettingsRow icon={User} label="Account" value={ssh?.user ?? 'Not set'} />
        <SettingsRow icon={KeyRound} label="Private key" value={ssh?.key ? 'Stored with the machine' : 'Not set'} />
      </SettingsGroup>
      <SettingsGroup>
        <SettingsRow
          as="form"
          onSubmit={(e) => {
            e.preventDefault()
            if (user.trim() && key.trim()) save.run()
          }}
          label="Set"
          control={
            <div className="flex min-w-0 flex-1 gap-2">
              <Input size="small" className="w-32 shrink-0" placeholder="Account" value={user} onChange={(e) => setUser(e.target.value)} />
              <Input size="small" className="min-w-0 flex-1 font-mono" placeholder="/Users/you/.ssh/win11_ed25519" value={key} onChange={(e) => setKey(e.target.value)} />
            </div>
          }
          actions={
            <Button type="submit" size="sm" variant="secondary" disabled={!user.trim() || !key.trim()} pending={save.pending}>
              Save
            </Button>
          }
        />
      </SettingsGroup>
    </SettingsSection>
  )
}
