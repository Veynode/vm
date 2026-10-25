import { useState } from 'react'
import { createFileRoute, useNavigate } from '@tanstack/react-router'
import { runContainer } from '../api'
import { lines, useAction, words } from '../ui'

export const Route = createFileRoute('/run')({
  validateSearch: (search: Record<string, unknown>): { image?: string } =>
    typeof search.image === 'string' ? { image: search.image } : {},
  component: Run,
})

function Run() {
  const { image: initial } = Route.useSearch()
  const navigate = useNavigate()
  const [form, setForm] = useState({
    image: initial ?? '',
    command: '',
    name: '',
    ports: '',
    volumes: '',
    env: '',
    cpus: '',
    memory: '',
    gpus: '',
    desktop: false,
    shared: false,
  })
  const set = (key: keyof typeof form) => (e: { target: { value: string; checked?: boolean; type?: string } }) =>
    setForm({ ...form, [key]: e.target.type === 'checkbox' ? e.target.checked : e.target.value })
  const run = useAction(async () => {
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
        shared: form.shared,
      },
    })
    if (form.shared) await navigate({ to: '/engine' })
    else await navigate({ to: '/containers/$id', params: { id: result.id } })
  })
  return (
    <>
      <h1>Run a container</h1>
      <form
        className="panel form"
        onSubmit={(e) => {
          e.preventDefault()
          run.run()
        }}
      >
        <label className="full">
          Image
          <input className="mono" required placeholder="nginx:alpine" value={form.image} onChange={set('image')} />
        </label>
        <label className="full">
          Command (optional, overrides the image Cmd)
          <input className="mono" placeholder={`sh -c 'echo hello'`} value={form.command} onChange={set('command')} />
        </label>
        <label>
          Name
          <input value={form.name} placeholder="auto" onChange={set('name')} />
        </label>
        <label>
          Ports (HOST:GUEST, comma separated)
          <input className="mono" placeholder="8080:80" value={form.ports} onChange={set('ports')} />
        </label>
        <label>
          CPUs
          <input type="number" min={1} max={32} placeholder="2" value={form.cpus} onChange={set('cpus')} />
        </label>
        <label>
          Memory
          <input placeholder="512m (2g with GPU)" value={form.memory} onChange={set('memory')} />
        </label>
        <label>
          GPU
          <select value={form.gpus} onChange={set('gpus')}>
            <option value="">None (Apple Virtualization)</option>
            <option value="virgl">virgl — OpenGL (QEMU/HVF)</option>
            <option value="venus">Venus — Vulkan via MoltenVK (experimental)</option>
          </select>
        </label>
        <div className="row" style={{ alignSelf: 'end', gap: 16 }}>
          <label className="check">
            <input type="checkbox" checked={form.desktop} onChange={set('desktop')} /> Desktop display
          </label>
          <label className="check">
            <input type="checkbox" checked={form.shared} onChange={set('shared')} /> In shared engine
          </label>
        </div>
        <label className="full">
          Volumes (one per line, /host/path:/container/path[:ro])
          <textarea rows={2} value={form.volumes} onChange={set('volumes')} />
        </label>
        <label className="full">
          Environment (one KEY=VALUE per line)
          <textarea rows={3} value={form.env} onChange={set('env')} />
        </label>
        <div className="full row">
          <button className="primary" disabled={run.pending}>{run.pending ? 'Starting…' : 'Run'}</button>
          <span className="muted">
            {form.shared
              ? 'Runs with docker run in the shared engine VM.'
              : form.gpus
                ? 'Boots a QEMU/HVF micro-VM with a virtio-gpu device.'
                : form.desktop
                  ? 'Boots an Apple Virtualization micro-VM with a 2D display (software rendering).'
                  : 'Boots an Apple Virtualization micro-VM.'}
          </span>
        </div>
        {run.error && <p className="error full">{run.error}</p>}
      </form>
    </>
  )
}
