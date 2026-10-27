import { useState } from 'react'
import { useRouter } from '@tanstack/react-router'
import { IconButton } from '@ui/components/ui/icon-button'
import { Box, Cpu, Palette, PenLine, Play, Square, Trash2 } from '@ui/icons'
import { ListRow } from '@ui/settings/list-row'
import { RowMenu } from '@ui/settings/row-menu'
import { SettingsGroup } from '@ui/settings/settings-section'
import { StateTag } from '@ui/settings/state-tag'
import { ContainerIcon } from './container-icon'
import { HardwareDialog, containerHardware, type Hardware } from './hardware-dialog'
import { IconDialog } from './icon-dialog'
import { RenameDialog } from './rename-dialog'
import { RunDialog } from './run-dialog'
import { containerAction, daemonStatus, inspectContainer, listContainers, type ContainerRow } from './api'
import { EngineSwitch } from './engine-switch'
import { ConfirmDialog, EmptyState, PageActions, PageBody, PanelLoadError, StatusTag, message, usePoll, useToastAction } from './ui'
import { useGo } from './page-nav'

export const isUp = (state: string) => !['stopped', 'crashed', 'missing', 'unknown'].includes(state)

/** What the Containers page reads: the containers, and the engine for the switch in its title band. */
export async function loadContainers() {
  const [containers, daemon] = await Promise.all([listContainers(), daemonStatus().catch(() => null)])
  return { containers, daemon }
}

export function ContainersError({ error, reset }: { error: unknown; reset: () => void }) {
  return <PanelLoadError title="Couldn't load the containers." detail={message(error)} onRetry={reset} />
}

/**
 * The containers, one row each: its state, its image, Start or Stop, and
 * Remove in its ··· menu. Drawn at `/containers` and, on a wider screen, at
 * `/` beside the places (where a phone shows the places alone).
 */
export function ContainersPage({ reading }: { reading: Awaited<ReturnType<typeof loadContainers>> }) {
  const { containers, daemon } = reading
  const router = useRouter()
  const go = useGo()
  usePoll(3000)
  const action = useToastAction(async (act: 'start' | 'stop' | 'rm', id: string) => {
    await containerAction({ data: { action: act, id } })
    await router.invalidate()
  })
  const [removing, setRemoving] = useState<ContainerRow | null>(null)
  const [running, setRunning] = useState(false)
  const [iconFor, setIconFor] = useState<ContainerRow | null>(null)
  const [renaming, setRenaming] = useState<ContainerRow | null>(null)
  const [hardware, setHardware] = useState<Hardware | null>(null)
  // The dialog needs what only `inspect` reads (the disk, DNS, MTU).
  const openHardware = useToastAction(async (c: ContainerRow) => {
    setHardware(containerHardware(await inspectContainer({ data: c.id }), isUp(c.state)))
  })
  return (
    <PageBody label="Containers">
      <PageActions>
        <EngineSwitch daemon={daemon} reading={reading} />
        <IconButton variant="ghost" icon={Play} aria-label="Run" title="Run a container" onClick={() => setRunning(true)} />
      </PageActions>
      {containers.length === 0 ? (
        <EmptyState icon={Box} title="No containers" />
      ) : (
        <SettingsGroup>
          {containers.map((c) => {
            const up = isUp(c.state)
            return (
              <ListRow
                key={c.id}
                icon={<ContainerIcon icon={c.icon} desktop={c.desktop} up={up} />}
                title={c.name}
                // Beside the name rather than as the row's `status`, which
                // gives way to dots when the name runs short: the name is cut
                // instead, so the state and GPU always read as words.
                titleAside={
                  <span className="ms-1 inline-flex h-5 shrink-0 items-center gap-1">
                    <StatusTag state={c.state} status={c.status} />
                    {c.gpus && <StateTag>GPU {c.gpus}</StateTag>}
                  </span>
                }
                meta={c.image}
                onSelect={() => go({ to: '/containers/$id', params: { id: c.id } })}
                actions={
                  <>
                    <IconButton size="sm" variant="ghost" icon={up ? Square : Play} aria-label={up ? `Stop ${c.name}` : `Start ${c.name}`} title={up ? 'Stop' : 'Start'} disabled={action.pending} onClick={() => action.run(up ? 'stop' : 'start', c.id)} />
                    <RowMenu label={`More for ${c.name}`} items={[
                        { label: 'Rename', icon: PenLine, onSelect: () => setRenaming(c) },
                        { label: 'Change icon', icon: Palette, onSelect: () => setIconFor(c) },
                        !c.engine && { label: 'Hardware', icon: Cpu, onSelect: () => openHardware.run(c) },
                        { label: 'Remove', icon: Trash2, danger: true, apart: true, onSelect: () => setRemoving(c) },
                      ]} />
                  </>
                }
              />
            )
          })}
        </SettingsGroup>
      )}
      <ConfirmDialog
        open={removing !== null}
        title={`Remove ${removing?.name ?? ''}?`}
        confirmLabel="Remove"
        busy={action.pending}
        onConfirm={async () => {
          if (removing) await action.run('rm', removing.id)
          setRemoving(null)
        }}
        close={() => setRemoving(null)}
      >
        Its virtual machine and writable layer are deleted.
      </ConfirmDialog>
      <RunDialog open={running} close={() => setRunning(false)} />
      <IconDialog container={iconFor} close={() => setIconFor(null)} />
      <RenameDialog container={renaming} close={() => setRenaming(null)} renamed={() => router.invalidate()} />
      <HardwareDialog hw={hardware} close={() => setHardware(null)} />
    </PageBody>
  )
}
