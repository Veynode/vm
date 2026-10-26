import { useEffect, useRef, useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import { DesktopClient, type DesktopStats } from '../desktop/client'

export const Route = createFileRoute('/desktop/$id')({
  validateSearch: (search: Record<string, unknown>) => ({
    codec: search.codec === 'h264' ? ('h264' as const) : ('hevc' as const),
  }),
  component: Desktop,
})

function Desktop() {
  const { id } = Route.useParams()
  const { codec } = Route.useSearch()
  const canvas = useRef<HTMLCanvasElement>(null)
  const client = useRef<DesktopClient | null>(null)
  const [stats, setStats] = useState<DesktopStats | null>(null)
  const [control, setControl] = useState(true)
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    if (!canvas.current) return
    if (typeof VideoDecoder === 'undefined') {
      setStats({ state: 'error', width: 0, height: 0, fps: 0, kbps: 0, decodeMs: 0, inputMs: null, message: 'This browser has no WebCodecs VideoDecoder.' })
      return
    }
    const c = new DesktopClient(canvas.current, id, codec)
    client.current = c
    c.control = control
    const off = c.onStats(setStats)
    c.connect()
    canvas.current.focus()
    return () => {
      off()
      c.close()
      client.current = null
    }
  }, [id, codec, attempt])

  useEffect(() => {
    if (client.current) client.current.control = control
  }, [control])

  const live = stats?.state === 'streaming'
  return (
    <div className="desktop-page">
      <div className="spread desktop-bar">
        <div className="row">
          <Link to="/containers/$id" params={{ id }}>← {id}</Link>
          <span className={`badge ${live ? 'up' : stats?.state === 'error' ? 'bad' : 'down'}`}>{stats?.state ?? 'connecting'}</span>
          {stats && live && (
            <span className="muted mono">
              {stats.width}×{stats.height} · {stats.codec?.toUpperCase()} · {stats.fps} fps · {stats.kbps} kbit/s · decode {stats.decodeMs} ms ·{' '}
              {stats.inputMs !== null && `input→frame ${stats.inputMs} ms · `}
              {stats.backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple VZ'}
            </span>
          )}
        </div>
        <div className="row">
          <label className="inline">
            <input type="checkbox" checked={control} onChange={(e) => setControl(e.target.checked)} /> Control
          </label>
          <Link to="/desktop/$id" params={{ id }} search={{ codec: codec === 'hevc' ? 'h264' : 'hevc' }}>
            {codec === 'hevc' ? 'Use H.264' : 'Use HEVC'}
          </Link>
          <button onClick={() => canvas.current?.requestFullscreen()}>Fullscreen</button>
          {(stats?.state === 'closed' || stats?.state === 'error') && <button onClick={() => setAttempt((n) => n + 1)}>Reconnect</button>}
        </div>
      </div>
      {stats?.message && <p className="error">{stats.message}</p>}
      {stats?.inputError && control && <p className="error">Input: {stats.inputError}</p>}
      <div className="desktop-frame">
        <canvas ref={canvas} tabIndex={0} className={control ? 'desktop-canvas control' : 'desktop-canvas'} />
      </div>
      <p className="muted">
        {control
          ? 'Keyboard and pointer go to the guest while the canvas has focus. Click outside to type elsewhere.'
          : 'View only. Tick Control to send keyboard and pointer input.'}
      </p>
    </div>
  )
}
