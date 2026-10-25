import { useEffect, useRef, useState } from 'react'
import { Link, createFileRoute, useNavigate, useRouter } from '@tanstack/react-router'
import { containerAction, containerLogs, execInContainer, inspectContainer, openDesktop } from '../api'
import { StatusBadge, useAction, usePoll, words } from '../ui'

export const Route = createFileRoute('/containers/$id')({
  loader: async ({ params }) => {
    const [info, logs] = await Promise.all([inspectContainer({ data: params.id }), containerLogs({ data: params.id })])
    return { info, logs }
  },
  component: Detail,
})

function Detail() {
  const { info, logs } = Route.useLoaderData()
  const router = useRouter()
  const navigate = useNavigate()
  usePoll(2000)
  const c = info.container
  const up = !['stopped', 'crashed', 'missing', 'unknown'].includes(info.state)
  const action = useAction(async (act: 'start' | 'stop' | 'rm') => {
    await containerAction({ data: { action: act, id: c.id } })
    if (act === 'rm') await navigate({ to: '/' })
    else await router.invalidate()
  })
  const desktop = useAction(() => openDesktop({ data: c.id }))
  const logRef = useRef<HTMLPreElement>(null)
  useEffect(() => {
    const el = logRef.current
    if (el && el.scrollHeight - el.scrollTop - el.clientHeight < 80) el.scrollTop = el.scrollHeight
  }, [logs])

  const [command, setCommand] = useState('')
  const [result, setResult] = useState<{ exit_code: number; stdout: string; stderr: string; timed_out: boolean } | null>(null)
  const exec = useAction(async () => {
    const argv = words(command)
    setResult(await execInContainer({ data: { id: c.id, argv } }))
  })

  return (
    <>
      <div className="spread">
        <div>
          <h1>
            {c.name} <StatusBadge state={info.state} status={info.status} />
          </h1>
          <div className="muted mono">{c.id} · {c.image}</div>
        </div>
        <div className="row">
          {c.desktop && up && (
            <button disabled={desktop.pending} onClick={() => desktop.run()}>Open desktop window</button>
          )}
          {up ? (
            <button disabled={action.pending} onClick={() => action.run('stop')}>Stop</button>
          ) : (
            <button disabled={action.pending} onClick={() => action.run('start')}>Start</button>
          )}
          <button className="danger" disabled={action.pending} onClick={() => confirm(`Remove ${c.name}?`) && action.run('rm')}>
            Remove
          </button>
        </div>
      </div>
      {(action.error || desktop.error) && <p className="error">{action.error || desktop.error}</p>}
      {info.errors?.length > 0 && <p className="error">{info.errors.join('\n')}</p>}
      <div className="grid2">
        <section className="panel">
          <h2 style={{ marginTop: 0 }}>Details</h2>
          <dl>
            <dt>Command</dt>
            <dd className="mono">{c.argv.join(' ')}</dd>
            <dt>Backend</dt>
            <dd>{c.backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple Virtualization'} · {c.cpus} CPU · {c.memory_mib} MiB</dd>
            <dt>Guest IP</dt>
            <dd className="mono">{info.ip ?? '—'}</dd>
            <dt>GPU</dt>
            <dd>{c.gpus ? `${c.gpus} (${info.gpu ?? 'not reported yet'})` : 'none'}</dd>
            <dt>Ports</dt>
            <dd className="mono">
              {c.ports.length ? c.ports.map((p: any) => `${p.host_address}:${p.host_port}→${p.guest_port}/${p.protocol}`).join(', ') : '—'}
            </dd>
            <dt>Volumes</dt>
            <dd className="mono">
              {c.volumes.length ? c.volumes.map((v: any) => `${v.host}:${v.target}${v.read_only ? ':ro' : ''}`).join(', ') : '—'}
            </dd>
            <dt>Exit code</dt>
            <dd>{info.exit_code ?? '—'}</dd>
            <dt>VM</dt>
            <dd className="mono">{c.vm_id}</dd>
          </dl>
        </section>
        <section className="panel">
          <h2 style={{ marginTop: 0 }}>Exec</h2>
          <form
            className="row"
            onSubmit={(e) => {
              e.preventDefault()
              if (command.trim()) exec.run()
            }}
          >
            <input
              className="mono"
              placeholder={up ? 'cat /etc/os-release' : 'container is not running'}
              value={command}
              disabled={!up}
              onChange={(e) => setCommand(e.target.value)}
              style={{ flex: 1 }}
            />
            <button className="primary" disabled={!up || exec.pending}>Run</button>
          </form>
          <p className="muted" style={{ margin: '6px 0 8px' }}>Runs argv directly (no shell), bounded to 30 s. Use sh -c '…' for pipelines.</p>
          {exec.error && <p className="error">{exec.error}</p>}
          {result && (
            <>
              <div className="muted">exit {result.exit_code}{result.timed_out ? ' (timed out)' : ''}</div>
              <pre className="log">{result.stdout}{result.stderr && <span style={{ color: 'var(--bad)' }}>{result.stderr}</span>}</pre>
            </>
          )}
        </section>
      </div>
      <h2>Logs</h2>
      <pre className="log" ref={logRef}>{logs || <span className="muted">No output yet.</span>}</pre>
      <p className="muted">
        <Link to="/">← All containers</Link>
      </p>
    </>
  )
}
