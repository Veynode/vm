import { useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { Button } from '@ui/components/ui/button'
import { Switch } from '@ui/components/ui/switch'
import { Copy, FolderOpen, Keyboard, Play, Terminal } from '@ui/icons'
import { SettingsRow } from '@ui/settings/settings-row'
import { SettingsGroup, SettingsSection } from '@ui/settings/settings-section'
import { systemInfo } from '../api'
import { CodeBlock, PageBody, PanelLoadError, message, useToast } from '../ui'

export const Route = createFileRoute('/mcp')({
  loader: () => systemInfo(),
  component: Mcp,
  errorComponent: ({ error, reset }) => <PanelLoadError title="Couldn't read this machine's setup." detail={message(error)} onRetry={reset} />,
})

const SCOPES = [
  { key: 'lifecycle', label: 'Lifecycle', icon: Play, note: 'Run, stop and remove containers; pull images; start VMs.' },
  { key: 'execute', label: 'Execute', icon: Terminal, note: 'Run commands inside guests (vm_exec).' },
  { key: 'files', label: 'Files', icon: FolderOpen, note: 'Upload and download single files.' },
  { key: 'input', label: 'Desktop input', icon: Keyboard, note: 'Click, drag and type on QEMU desktops after a screenshot.' },
] as const

/** What an MCP client may do, and the configuration that grants it. */
function Mcp() {
  const info = Route.useLoaderData()
  const toast = useToast()
  const [scopes, setScopes] = useState<Record<string, boolean>>({ lifecycle: true })
  const args = ['mcp', ...SCOPES.filter((s) => scopes[s.key]).map((s) => `--${s.key}`)]
  const config = JSON.stringify(
    {
      mcpServers: {
        veynodevm: {
          command: info.cli,
          args,
          ...(info.home ? { env: { VEYNODEVM_HOME: info.home } } : {}),
        },
      },
    },
    null,
    2,
  )
  return (
    <PageBody label="MCP">
      <SettingsSection title="Scopes">
        <SettingsGroup footnote="Observation tools (list, inspect, logs, screenshots) are always available. Grant more only as needed.">
          {SCOPES.map((s) => (
            <SettingsRow
              key={s.key}
              icon={s.icon}
              label={s.label}
              hint={s.note}
              control={<Switch checked={!!scopes[s.key]} onCheckedChange={(on) => setScopes({ ...scopes, [s.key]: on })} />}
            />
          ))}
        </SettingsGroup>
      </SettingsSection>
      <SettingsSection
        title="Client configuration"
        actions={
          <Button
            size="sm"
            variant="secondary"
            leadingIcon={Copy}
            onClick={() =>
              navigator.clipboard.writeText(config).then(
                () => toast('Copied'),
                (e) => toast(message(e), 'error'),
              )
            }
          >
            Copy
          </Button>
        }
        description="Containers expose their backing VM id, so vm_screenshot, vm_exec and the input tools work on micro-VM containers too."
      >
        <CodeBlock>{config}</CodeBlock>
      </SettingsSection>
    </PageBody>
  )
}
