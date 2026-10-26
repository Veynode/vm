import { useEffect, useRef, useState } from 'react'
import { Link, createFileRoute } from '@tanstack/react-router'
import '@xterm/xterm/css/xterm.css'

export const Route = createFileRoute('/terminal/$id')({
  component: Terminal,
})

type State = { state: 'connecting' | 'ready' | 'exited' | 'closed' | 'error'; message?: string }

function Terminal() {
  const { id } = Route.useParams()
  const host = useRef<HTMLDivElement>(null)
  const [status, setStatus] = useState<State>({ state: 'connecting' })
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    if (!host.current) return
    let disposed = false
    let cleanup = () => {}
    // xterm.js needs the DOM; load it in the browser only.
    void Promise.all([import('@xterm/xterm'), import('@xterm/addon-fit')]).then(([{ Terminal }, { FitAddon }]) => {
      if (disposed || !host.current) return
      const term = new Terminal({ cursorBlink: true, fontFamily: 'ui-monospace, SFMono-Regular, Menlo, monospace', fontSize: 13, scrollback: 5000 })
      const fit = new FitAddon()
      term.loadAddon(fit)
      term.open(host.current)
      fit.fit()
      const url = new URL('/terminal/ws', location.href)
      url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
      url.searchParams.set('id', id)
      url.searchParams.set('rows', String(term.rows))
      url.searchParams.set('cols', String(term.cols))
      const socket = new WebSocket(url)
      socket.binaryType = 'arraybuffer'
      setStatus({ state: 'connecting' })
      let ended = false
      socket.onmessage = (event) => {
        if (typeof event.data !== 'string') {
          term.write(new Uint8Array(event.data))
          return
        }
        const message = JSON.parse(event.data) as { event: string; code?: number; message?: string }
        if (message.event === 'ready') {
          setStatus({ state: 'ready' })
          term.focus()
        } else if (message.event === 'exit') {
          ended = true
          setStatus({ state: 'exited', message: `Shell exited with status ${message.code}.` })
        } else if (message.event === 'error') {
          ended = true
          setStatus({ state: 'error', message: message.message })
        }
      }
      socket.onclose = () => {
        if (!ended) setStatus({ state: 'closed', message: 'Connection closed.' })
      }
      const send = (value: unknown) => socket.readyState === WebSocket.OPEN && socket.send(JSON.stringify(value))
      const input = term.onData((data) => send({ input: data }))
      const resize = term.onResize(({ rows, cols }) => send({ resize: { rows, cols } }))
      const observer = new ResizeObserver(() => fit.fit())
      observer.observe(host.current)
      cleanup = () => {
        observer.disconnect()
        input.dispose()
        resize.dispose()
        socket.close()
        term.dispose()
      }
    })
    return () => {
      disposed = true
      cleanup()
    }
  }, [id, attempt])

  const badge = status.state === 'ready' ? 'up' : status.state === 'error' ? 'bad' : 'down'
  return (
    <div className="terminal-page">
      <div className="spread desktop-bar">
        <div className="row">
          <Link to="/containers/$id" params={{ id }}>← {id}</Link>
          <span className={`badge ${badge}`}>{status.state}</span>
        </div>
        {status.state !== 'ready' && status.state !== 'connecting' && (
          <button onClick={() => setAttempt((n) => n + 1)}>New session</button>
        )}
      </div>
      {status.message && <p className={status.state === 'error' ? 'error' : 'muted'}>{status.message}</p>}
      <div className="terminal-frame" ref={host} />
    </div>
  )
}
