import { createFileRoute, useRouter } from '@tanstack/react-router'
import { engineAction, engineStatus } from '../api'
import { StatusBadge, useAction, usePoll } from '../ui'

export const Route = createFileRoute('/engine')({
  loader: () => engineStatus(),
  component: Engine,
})

function Engine() {
  const status = Route.useLoaderData()
  const router = useRouter()
  usePoll(4000)
  const action = useAction(async (act: 'start' | 'stop') => {
    await engineAction({ data: act })
    await router.invalidate()
  })
  const exists = status.state !== 'absent'
  const up = exists && !['stopped', 'crashed', 'missing'].includes(status.state)
  return (
    <>
      <div className="spread">
        <h1>
          Shared engine <StatusBadge state={status.state} status={status.status} />
        </h1>
        <div className="row">
          {up ? (
            <button disabled={action.pending} onClick={() => action.run('stop')}>Stop</button>
          ) : (
            <button className="primary" disabled={action.pending} onClick={() => action.run('start')}>
              {action.pending ? 'Starting…' : exists ? 'Start' : 'Create and start'}
            </button>
          )}
        </div>
      </div>
      {action.error && <p className="error">{action.error}</p>}
      <div className="grid2">
        <section className="panel">
          <h2 style={{ marginTop: 0 }}>What it is</h2>
          <p>
            One long-lived Linux micro-VM running dockerd (docker:dind). The stock docker CLI, docker build and compose talk to
            it over mutual TLS on host loopback. Ports published by its containers are forwarded to 127.0.0.1 automatically.
          </p>
          {exists && (
            <dl>
              <dt>Endpoint</dt>
              <dd className="mono">{status.endpoint}</dd>
              <dt>Backend</dt>
              <dd>{status.backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple Virtualization'}</dd>
              <dt>GPU (CDI)</dt>
              <dd>{status.gpu ?? 'none'}</dd>
            </dl>
          )}
        </section>
        <section className="panel">
          <h2 style={{ marginTop: 0 }}>Use it</h2>
          <pre className="log">{`docker --context ${status.context ?? 'veynodevm'} ps
docker --context veynodevm run --rm -p 8080:80 nginx
docker --context veynodevm compose up -d

# or point the current shell at it
eval "$(veynodevm engine env)"

# GPU containers need the engine started with --gpus
docker run --device veynodevm.dev/gpu=all …`}</pre>
        </section>
      </div>
    </>
  )
}
