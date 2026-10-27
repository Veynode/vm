import { useState } from 'react'
import { createFileRoute, useRouter } from '@tanstack/react-router'
import { Button } from '@ui/components/ui/button'
import { IconButton } from '@ui/components/ui/icon-button'
import { Monitor, MonitorCog, Terminal, Palette, PenLine, Play, Plus, Power, Square, Trash2 } from '@ui/icons'
import { ListRow } from '@ui/settings/list-row'
import { RowMenu } from '@ui/settings/row-menu'
import { SettingsGroup } from '@ui/settings/settings-section'
import { StateTag } from '@ui/settings/state-tag'
import { MachineIcon } from '../container-icon'
import { MachineIconDialog } from '../icon-dialog'
import { NewMachineDialog } from '../machine-dialog'
import { RenameMachineDialog } from '../rename-dialog'
import { daemonStatus, listMachines, machineAction, type MachineRow } from '../api'
import { EngineSwitch } from '../engine-switch'
import type { MachineAction } from '../machine-input.mjs'
import { ConfirmDialog, EmptyState, PageActions, PageBody, PanelLoadError, StatusTag, message, size, usePoll, useToastAction } from '../ui'
import { useGo } from '../page-nav'
import { DesktopDialog, TerminalDialog, type ConsoleTarget } from '../console-dialog'

export const Route = createFileRoute('/machines')({
  // The engine is read beside them for the switch in the title band.
  loader: async () => {
    const [machines, daemon] = await Promise.all([listMachines(), daemonStatus().catch(() => null)])
    return { machines, daemon }
  },
  component: Machines,
  errorComponent: ({ error, reset }) => <PanelLoadError title="Couldn't load the VMs." detail={message(error)} onRetry={reset} />,
})

/** `machine.rs` `running`: anything but these is up (starting, running, stopping). */
export const machineUp = (state: string) => !['stopped', 'crashed', 'missing', 'unknown', 'invalid'].includes(state)

export const osLabel = (os: string) => (os === 'windows' ? 'Windows' : 'Linux')
export const backendLabel = (backend: string) => (backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple Virtualization')

/** CPU, memory and disk (used of its size) in one line. */
export function resources(row: { cpus?: number; memory_mib?: number; disk_used_bytes?: number; machine: { disk_gib: number } }) {
  const parts = []
  if (row.cpus) parts.push(`${row.cpus} CPU`)
  if (row.memory_mib) parts.push(row.memory_mib % 1024 === 0 ? `${row.memory_mib / 1024} GiB` : `${row.memory_mib} MiB`)
  parts.push(row.disk_used_bytes !== undefined ? `${size(row.disk_used_bytes)} of ${row.machine.disk_gib} GB disk` : `${row.machine.disk_gib} GB disk`)
  return parts.join(' · ')
}

/**
 * The machines (`veynodevm machine ls`): full Linux distributions and
 * Windows installed from their own ISO. One row each: state, system and
 * backend, resources, Terminal (over SSH) and Desktop when it has them, Start or Stop; Rename, Change icon, Force stop and
 * Remove in its ··· menu.
 * macOS VMs are containers and stay under Containers.
 */
function Machines() {
  const reading = Route.useLoaderData()
  const { machines, daemon } = reading
  const router = useRouter()
  const go = useGo()
  usePoll(3000)
  const action = useToastAction(async (act: MachineAction, name: string) => {
    await machineAction({ data: { action: act, name } })
    await router.invalidate()
  })
  const [creating, setCreating] = useState(false)
  const [removing, setRemoving] = useState<MachineRow | null>(null)
  const [killing, setKilling] = useState<MachineRow | null>(null)
  const [renaming, setRenaming] = useState<MachineRow | null>(null)
  const [changingIcon, setChangingIcon] = useState<MachineRow | null>(null)
  const [terminal, setTerminal] = useState<ConsoleTarget | null>(null)
  const [desktop, setDesktop] = useState<ConsoleTarget | null>(null)
  return (
    <PageBody label="VMs">
      <PageActions>
        <EngineSwitch daemon={daemon} reading={reading} />
        <IconButton variant="ghost" icon={Plus} aria-label="New VM" title="New VM" onClick={() => setCreating(true)} />
      </PageActions>
      {machines.length === 0 ? (
        <EmptyState icon={MonitorCog} title="No VMs" action={<Button variant="secondary" onClick={() => setCreating(true)}>New VM</Button>} />
      ) : (
        <SettingsGroup>
          {machines.map((m) => {
            const up = machineUp(m.state)
            const name = m.machine.name
            return (
              <ListRow
                key={name}
                icon={<MachineIcon icon={m.machine.icon} up={up} />}
                title={name}
                // Beside the name, as on Containers: the name is cut before
                // the state and ISO tags give way to dots.
                titleAside={
                  <span className="ms-1 inline-flex h-5 shrink-0 items-center gap-1">
                    <StatusTag state={m.state} status={m.state} />
                    {m.machine.iso && <StateTag>ISO</StateTag>}
                  </span>
                }
                meta={`${osLabel(m.machine.os)} · ${backendLabel(m.machine.backend)} · ${resources(m)}`}
                onSelect={() => go({ to: '/machines/$id', params: { id: m.machine.id } })}
                actions={
                  <>
                    {up && m.terminal && (
                      <IconButton size="sm" variant="ghost" icon={Terminal} aria-label={`Terminal of ${name}`} title="Terminal" onClick={() => setTerminal({ id: m.machine.id, name })} />
                    )}
                    {up && m.desktop && (
                      <IconButton size="sm" variant="ghost" icon={Monitor} aria-label={`Desktop of ${name}`} title="Desktop" onClick={() => setDesktop({ id: m.machine.id, name })} />
                    )}
                    <IconButton size="sm" variant="ghost" icon={up ? Square : Play} aria-label={up ? `Stop ${name}` : `Start ${name}`} title={up ? 'Stop (ACPI power button)' : 'Start'} disabled={action.pending} onClick={() => action.run(up ? 'stop' : 'start', name)} />
                    <RowMenu
                      label={`More for ${name}`}
                      items={[
                        { label: 'Rename', icon: PenLine, disabled: up, onSelect: () => setRenaming(m) },
                        { label: 'Change icon', icon: Palette, onSelect: () => setChangingIcon(m) },
                        up && { label: 'Force stop', icon: Power, onSelect: () => setKilling(m) },
                        { label: 'Remove', icon: Trash2, danger: true, apart: true, disabled: up, onSelect: () => setRemoving(m) },
                      ]}
                    />
                  </>
                }
              />
            )
          })}
        </SettingsGroup>
      )}
      <ConfirmDialog
        open={killing !== null}
        title={`Force stop ${killing?.machine.name ?? ''}?`}
        confirmLabel="Force stop"
        busy={action.pending}
        onConfirm={async () => {
          if (killing) await action.run('kill', killing.machine.name)
          setKilling(null)
        }}
        close={() => setKilling(null)}
      >
        The VM is killed like pulling its power cord; anything the guest has not written to disk is lost.
      </ConfirmDialog>
      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing?.machine.name ?? ''}?`}
        confirmLabel="Remove"
        busy={action.pending}
        onConfirm={async () => {
          if (removing) await action.run('rm', removing.machine.name)
          setRemoving(null)
        }}
        close={() => setRemoving(null)}
      >
        Its disk, UEFI variables and serial log are deleted. The installer ISO and attached images on this Mac are left alone.
      </ConfirmDialog>
      <NewMachineDialog open={creating} close={() => setCreating(false)} />
      <RenameMachineDialog machine={renaming && renaming.machine} close={() => setRenaming(null)} renamed={() => router.invalidate()} />
      <MachineIconDialog machine={changingIcon && changingIcon.machine} close={() => setChangingIcon(null)} />
      <TerminalDialog target={terminal} close={() => setTerminal(null)} />
      <DesktopDialog target={desktop} close={() => setDesktop(null)} />
    </PageBody>
  )
}
