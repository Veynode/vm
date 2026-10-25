import { useState } from 'react'
import { createFileRoute } from '@tanstack/react-router'
import { systemInfo } from '../api'

export const Route = createFileRoute('/mcp')({
  loader: () => systemInfo(),
  component: Mcp,
})

const SCOPES = [
  { key: 'lifecycle', label: 'Lifecycle', note: 'run, stop and remove containers; pull images; start VMs' },
  { key: 'execute', label: 'Execute', note: 'run commands inside guests (vm_exec)' },
  { key: 'files', label: 'Files', note: 'upload and download single files' },
  { key: 'input', label: 'Desktop input', note: 'click, drag and type on QEMU desktops after a screenshot' },
] as const

function Mcp() {
  const info = Route.useLoaderData()
  const [scopes, setScopes] = useState<Record<string, boolean>>({ lifecycle: true })
  const args = ['mcp', ...SCOPES.filter((s) => scopes[s.key]).map((s) => `--${s.key}`)]
  const config = {
    mcpServers: {
      veynodevm: {
        command: info.cli,
        args,
        ...(info.home ? { env: { VEYNODEVM_HOME: info.home } } : {}),
      },
    },
  }
  return (
    <>
      <h1>MCP</h1>
      <div className="grid2">
        <section className="panel">
          <h2 style={{ marginTop: 0 }}>Scopes</h2>
          <p className="muted">Observation tools (list, inspect, logs, screenshots) are always available. Grant more only as needed.</p>
          {SCOPES.map((s) => (
            <label key={s.key} className="check" style={{ margin: '8px 0' }}>
              <input
                type="checkbox"
                checked={!!scopes[s.key]}
                onChange={(e) => setScopes({ ...scopes, [s.key]: e.target.checked })}
              />
              <span>
                <b>{s.label}</b> <span className="muted">— {s.note}</span>
              </span>
            </label>
          ))}
        </section>
        <section className="panel">
          <h2 style={{ marginTop: 0 }}>Client configuration (stdio)</h2>
          <pre className="log">{JSON.stringify(config, null, 2)}</pre>
          <p className="muted">
            Containers expose their backing VM id, so vm_screenshot, vm_exec and input tools work on micro-VM containers too.
          </p>
        </section>
      </div>
    </>
  )
}
