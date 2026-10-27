import { useEffect, useMemo, useRef, useState } from 'react'
import { createFileRoute, useRouter } from '@tanstack/react-router'
import { Button } from '@ui/components/ui/button'
import { Input } from '@ui/components/ui/input'
import { AppWindow, ExternalLink, CircleDot, Globe, HardDrive, Hash, Layers, Monitor, Palette, PenLine, Server, SquareTerminal, Terminal, Trash2 } from '@ui/icons'
import { Notice } from '@ui/settings/notice'
import { RowIcon } from '@ui/settings/list-row'
import { RowMenu } from '@ui/settings/row-menu'
import { SettingsLinkRow } from '@ui/settings/settings-link-row'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup, SettingsSection } from '@ui/settings/settings-section'
import { containerAction, containerLogs, execInContainer, inspectContainer, openDesktop } from '../api'
import { CodeBlock, ConfirmDialog, PageActions, PageBody, PanelLoadError, StatusTag, message, usePoll, useSubPage, useToastAction, words } from '../ui'
import { HardwareSection, containerHardware } from '../hardware-dialog'
import { ContainerIcon } from '../container-icon'
import { IconDialog } from '../icon-dialog'
import { RenameDialog } from '../rename-dialog'
import { isUp } from '../containers-list'
import { useGo } from '../page-nav'
import { DesktopDialog, TerminalDialog } from '../console-dialog'

export const Route = createFileRoute('/containers_/$id')({
  loader: async ({ params }) => {
    const [info, logs] = await Promise.all([inspectContainer({ data: params.id }), containerLogs({ data: params.id })])
    return { info, logs }
  },
  component: Detail,
  errorComponent: ({ error, reset }) => <PanelLoadError title="Couldn't load this container." detail={message(error)} onRetry={reset} />,
})

/** A row whose value is text: cut on one line, the whole of it in its title. */
function Value({ children, mono }: { children: string; mono?: boolean }) {
  return (
    <span title={children} className={mono ? 'font-mono' : undefined}>
      {children}
    </span>
  )
}

/**
 * A container's own page under Containers: its state with Start/Stop, its
 * terminal, desktop and native window as rows, the rest in its ··· menu;
 * then its details, Exec and its logs.
 */
function Detail() {
  const { info, logs } = Route.useLoaderData()
  const router = useRouter()
  const go = useGo()
  usePoll(2000)
  const c = info.container
  const up = isUp(info.state)
  // Its name in the title band after its icon, as core's `PageTitle`; kept
  // the same node between polls so the band is not set again each time.
  const heading = useMemo(
    () => (
      <span className="flex min-w-0 items-center gap-2.5">
        <ContainerIcon icon={c.icon} desktop={c.desktop} up={up} />
        <span className="truncate">{c.name}</span>
      </span>
    ),
    [c.icon, c.desktop, c.name, up],
  )
  useSubPage(heading, () => go({ to: '/containers' }))
  const action = useToastAction(async (act: 'start' | 'stop' | 'rm') => {
    await containerAction({ data: { action: act, id: c.id } })
    if (act === 'rm') go({ to: '/containers' })
    else await router.invalidate()
  })
  const [removing, setRemoving] = useState(false)
  const [changingIcon, setChangingIcon] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [opened, setOpened] = useState<'terminal' | 'desktop' | null>(null)
  // macOS containers always have a display, streamed from the Guest's Screen Sharing.
  const macos = c.os === 'darwin'
  const nativeWindow = useToastAction(async () => {
    await openDesktop({ data: c.id })
  })
  const logRef = useRef<HTMLPreElement>(null)
  useEffect(() => {
    const el = logRef.current
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 80) el.scrollTop = el.scrollHeight
  }, [logs])

  const [command, setCommand] = useState('')
  const [result, setResult] = useState<{ exit_code: number; stdout: string; stderr: string; timed_out: boolean } | null>(null)
  const exec = useToastAction(async () => {
    setResult(await execInContainer({ data: { id: c.id, argv: words(command) } }))
  })

  const volumes = c.volumes.map((v: any) => `${v.host}:${v.target}${v.read_only ? ':ro' : ''}`).join(', ')
  return (
    <PageBody label={c.name}>
      <PageActions>
        <RowMenu
          label={`More for ${c.name}`}
          items={[
            { label: 'Rename', icon: PenLine, onSelect: () => setRenaming(true) },
            { label: 'Change icon', icon: Palette, onSelect: () => setChangingIcon(true) },
            { label: 'Remove', icon: Trash2, danger: true, apart: true, onSelect: () => setRemoving(true) },
          ]}
        />
      </PageActions>
      {info.errors?.length > 0 && <Notice status="error">{info.errors.join('\n')}</Notice>}
      <SettingsGroup>
        <SettingsRow
          icon={CircleDot}
          label="State"
          state={<StatusTag state={info.state} status={info.status} />}
          value={info.exit_code != null ? `exit ${info.exit_code}` : undefined}
          actions={
            <Button size="sm" variant="secondary" pending={action.pending} onClick={() => action.run(up ? 'stop' : 'start')}>
              {up ? 'Stop' : 'Start'}
            </Button>
          }
        />
        {/* --init systemd containers have no interactive session (docs/known-issues.md). */}
        {up && !c.init && <SettingsLinkRow icon={<RowIcon icon={Terminal} />} label="Terminal" meta={macos ? 'zsh over SSH' : 'Shell in the container'} onOpen={() => setOpened('terminal')} />}
        {up && (c.desktop || macos) && (
          <SettingsLinkRow icon={<RowIcon icon={Monitor} />} label="Desktop" meta="In this browser" onOpen={() => setOpened('desktop')} />
        )}
        {up && (c.desktop || macos) && (
          // Opens a window on this Mac; the window itself closes it.
          <SettingsLinkRow
            icon={<RowIcon icon={AppWindow} />}
            label="Native window"
            meta={'On this Mac'}
            value={<ExternalLink className="size-4 text-ink-2" aria-hidden />}
            chevron={false}
            disabled={nativeWindow.pending}
            onOpen={() => nativeWindow.run()}
          />
        )}
      </SettingsGroup>
      <SettingsGroup>
        <SettingsRow icon={Layers} label="Image" value={<Value mono>{c.image}</Value>} />
        <SettingsRow icon={SquareTerminal} label="Command" value={<Value mono>{c.argv.join(' ')}</Value>} />
        <SettingsRow icon={Globe} label="Guest IP" value={<Value mono>{info.ip ?? '—'}</Value>} />
        <SettingsRow icon={HardDrive} label="Volumes" value={<Value mono>{volumes || '—'}</Value>} />
        <SettingsRow icon={Server} label="VM" value={<Value mono>{c.vm_id}</Value>} />
        <SettingsRow icon={Hash} label="ID" value={<Value mono>{c.id}</Value>} />
      </SettingsGroup>
      <HardwareSection hw={containerHardware(info, up)} />
      <SettingsSection title="Exec" description="Runs argv directly (no shell), for at most 30 s. Use sh -c '…' for pipelines.">
        <SettingsGroup>
          <SettingsRow
            as="form"
            onSubmit={(e) => {
              e.preventDefault()
              if (command.trim()) exec.run()
            }}
            label="Command"
            control={
              <Input
                size="small"
                className="font-mono"
                placeholder={up ? 'cat /etc/os-release' : 'Not running'}
                value={command}
                disabled={!up}
                onChange={(e) => setCommand(e.target.value)}
              />
            }
            actions={
              <Button type="submit" size="sm" variant="secondary" disabled={!up || !command.trim()} pending={exec.pending}>
                Run
              </Button>
            }
          />
        </SettingsGroup>
        {result && (
          <CodeBlock>
            {result.stdout}
            {result.stderr && <span className="text-state-failed-ink">{result.stderr}</span>}
            <span className="text-ink-2">{`\nexit ${result.exit_code}${result.timed_out ? ' (timed out)' : ''}`}</span>
          </CodeBlock>
        )}
      </SettingsSection>
      <SettingsSection title="Logs">
        <CodeBlock ref={logRef}>{logs || <span className="text-ink-2">No output yet.</span>}</CodeBlock>
      </SettingsSection>
      <IconDialog container={changingIcon ? c : null} close={() => setChangingIcon(false)} />
      <RenameDialog container={renaming ? c : null} close={() => setRenaming(false)} renamed={() => router.invalidate()} />
      <ConfirmDialog
        open={removing}
        title={`Remove ${c.name}?`}
        confirmLabel="Remove"
        busy={action.pending}
        onConfirm={async () => {
          await action.run('rm')
          setRemoving(false)
        }}
        close={() => setRemoving(false)}
      >
        Its virtual machine and writable layer are deleted.
      </ConfirmDialog>
      <TerminalDialog target={opened === 'terminal' ? { id: c.id, name: c.name } : null} close={() => setOpened(null)} />
      <DesktopDialog target={opened === 'desktop' ? { id: c.id, name: c.name } : null} close={() => setOpened(null)} />
    </PageBody>
  )
}
