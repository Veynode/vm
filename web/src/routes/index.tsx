import { Link, createFileRoute, useRouter } from '@tanstack/react-router'
import { containerAction, listContainers } from '../api'
import { StatusBadge, age, useAction, usePoll } from '../ui'

export const Route = createFileRoute('/')({
  loader: () => listContainers(),
  component: Containers,
})

function Containers() {
  const containers = Route.useLoaderData()
  const router = useRouter()
  usePoll(3000)
  const action = useAction(async (act: 'start' | 'stop' | 'rm', id: string) => {
    await containerAction({ data: { action: act, id } })
    await router.invalidate()
  })
  return (
    <>
      <div className="spread">
        <h1>Containers</h1>
        <Link to="/run" className="button">Run a container</Link>
      </div>
      {action.error && <p className="error">{action.error}</p>}
      {containers.length === 0 ? (
        <div className="panel empty">No containers yet. Each container is its own micro-VM; GPU containers use QEMU/HVF.</div>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Image</th>
                <th>Status</th>
                <th>Ports</th>
                <th>Backend</th>
                <th>Created</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {containers.map((c) => {
                const up = !['stopped', 'crashed', 'missing', 'unknown'].includes(c.state)
                return (
                  <tr key={c.id}>
                    <td>
                      <Link to="/containers/$id" params={{ id: c.id }}>{c.name}</Link>
                      <div className="muted mono">{c.id}</div>
                    </td>
                    <td className="mono">{c.image}</td>
                    <td>
                      <StatusBadge state={c.state} status={c.status} />{' '}
                      {c.gpus && <span className="badge gpu">GPU {c.gpus}</span>}
                    </td>
                    <td className="mono">
                      {c.ports.map((p) => (
                        <div key={`${p.host_port}/${p.protocol}`}>
                          {p.protocol === 'tcp' && up ? (
                            <a href={`http://127.0.0.1:${p.host_port}/`} target="_blank" rel="noreferrer">
                              {p.host_port}→{p.guest_port}
                            </a>
                          ) : (
                            `${p.host_port}→${p.guest_port}/${p.protocol}`
                          )}
                        </div>
                      ))}
                    </td>
                    <td className="muted">{c.backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple VZ'}</td>
                    <td className="muted">{age(c.created_at)}</td>
                    <td>
                      <div className="row">
                        {up ? (
                          <button disabled={action.pending} onClick={() => action.run('stop', c.id)}>Stop</button>
                        ) : (
                          <button disabled={action.pending} onClick={() => action.run('start', c.id)}>Start</button>
                        )}
                        <button
                          className="danger"
                          disabled={action.pending}
                          onClick={() => confirm(`Remove ${c.name} and its writable layer?`) && action.run('rm', c.id)}
                        >
                          Remove
                        </button>
                      </div>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
  )
}
