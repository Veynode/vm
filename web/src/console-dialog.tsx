import { useEffect, useRef, useState, type ReactNode } from 'react'
import '@xterm/xterm/css/xterm.css'
import { AnimatedIcon } from '@ui/boardui/icons/animated-icon'
import { Button } from '@ui/components/ui/button'
import { CloseButton } from '@ui/components/ui/close-button'
import { Dialog, DialogClose, DialogContent, DialogHeader, DialogTitle } from '@ui/components/ui/dialog'
import { IconButton } from '@ui/components/ui/icon-button'
import { Film, Maximize } from '@ui/icons'
import { Notice } from '@ui/settings/notice'
import { RowMenu } from '@ui/settings/row-menu'
import { StateTag } from '@ui/settings/state-tag'
import { classes } from '@ui/settings/recipes'
import { DesktopClient, type DesktopStats } from './desktop/client'

/** A container or VM to open: the id the stream and terminal sockets take, and its name for the title. */
export type ConsoleTarget = { id: string; name: string }

/**
 * A wide dialog over the page it was opened from, framed as core's computer
 * window (`Modal variant="window"`): a 56px head with the title in title-3,
 * then the screen, dark in either theme, a 12px step in. Escape and the scrim
 * do not close it: Escape belongs to the guest (vi, a dialog in Windows), and
 * a stray click outside should not end a session. Its X does.
 */
function ConsoleDialog({ open, close, title, aside, actions, children }: { open: boolean; close: () => void; title: string; aside?: ReactNode; actions?: ReactNode; children: ReactNode }) {
  return (
    <Dialog
      open={open}
      disablePointerDismissal
      onOpenChange={(next, details) => {
        if (next) return
        if (details.reason === 'escape-key') details.cancel()
        else close()
      }}
    >
      <DialogContent className={classes('modal wide')}>
        <DialogHeader className="modal-head h-14 min-h-14 px-4! pt-0! pb-0! [&_h2]:text-title-3-semibold!">
          <DialogTitle className="shrink-0">{title}</DialogTitle>
          <span className="min-w-0 flex-1 truncate font-mono text-caption-1-regular text-ink-3 max-narrow:hidden">{aside}</span>
          <div className="flex shrink-0 items-center gap-1">
            {actions}
            <DialogClose render={<CloseButton size="sm" aria-label="Close" />} />
          </div>
        </DialogHeader>
        <div className="flex min-h-0 flex-1 flex-col gap-3 px-3 pb-3 max-narrow:px-2 max-narrow:pb-2">{children}</div>
      </DialogContent>
    </Dialog>
  )
}

/** The guest's desktop in this browser (WebCodecs over `/desktop/ws`), with input. */
export function DesktopDialog({ target, close }: { target: ConsoleTarget | null; close: () => void }) {
  const [codec, setCodec] = useState<'hevc' | 'h264'>('hevc')
  const [stats, setStats] = useState<DesktopStats | null>(null)
  const canvas = useRef<HTMLCanvasElement>(null)
  // What the stream is doing, beside the title, while there is a picture.
  const aside = stats?.state === 'streaming' && (
    <>
      {stats.width}×{stats.height} · {stats.codec?.toUpperCase()} · {stats.fps} fps · {stats.kbps} kbit/s · decode {stats.decodeMs} ms
      {stats.inputMs !== null && ` · input→frame ${stats.inputMs} ms`} · {stats.backend === 'qemu-hvf' ? 'QEMU/HVF' : 'Apple VZ'}
    </>
  )
  return (
    <ConsoleDialog
      open={!!target}
      close={close}
      title={target ? `${target.name} · Desktop` : ''}
      aside={aside}
      actions={
        <>
          <IconButton size="sm" variant="ghost" icon={Maximize} aria-label="Fullscreen" title="Fullscreen" onClick={() => canvas.current?.requestFullscreen()} />
          <RowMenu label="Stream" items={[{ label: codec === 'hevc' ? 'Use H.264' : 'Use HEVC', icon: Film, onSelect: () => setCodec(codec === 'hevc' ? 'h264' : 'hevc') }]} />
        </>
      }
    >
      {target && <Desktop id={target.id} name={target.name} codec={codec} canvas={canvas} stats={stats} setStats={setStats} />}
    </ConsoleDialog>
  )
}

function Desktop({
  id,
  name,
  codec,
  canvas,
  stats,
  setStats,
}: {
  id: string
  name: string
  codec: 'hevc' | 'h264'
  canvas: React.RefObject<HTMLCanvasElement | null>
  stats: DesktopStats | null
  setStats: (stats: DesktopStats | null) => void
}) {
  const [attempt, setAttempt] = useState(0)

  useEffect(() => {
    if (!canvas.current) return
    if (typeof VideoDecoder === 'undefined') {
      setStats({ state: 'error', width: 0, height: 0, fps: 0, kbps: 0, decodeMs: 0, inputMs: null, message: 'This browser has no WebCodecs VideoDecoder.' })
      return
    }
    const c = new DesktopClient(canvas.current, id, codec)
    c.control = true
    const off = c.onStats(setStats)
    c.connect()
    canvas.current.focus()
    return () => {
      off()
      c.close()
    }
  }, [id, codec, attempt, canvas, setStats])

  const live = stats?.state === 'streaming'
  const ended = stats?.state === 'closed' || stats?.state === 'error'
  return (
    <>
      {stats?.inputError && <Notice status="error">{`Input: ${stats.inputError}`}</Notice>}
      <div className="relative flex min-h-0 flex-1 items-center justify-center overflow-hidden rounded-lg border border-line bg-black">
        <canvas ref={canvas} tabIndex={0} aria-label={`${name} desktop`} className="desktop-canvas control max-h-full" />
        {/* Until the first frame, the picture's place says where the stream is. */}
        {!live && (
          <div role="status" className="absolute inset-0 flex flex-col items-center justify-center gap-3 p-6 text-center text-body-regular text-white/70">
            {ended ? (
              <>
                <span className="max-w-md">{stats?.message ?? 'The stream closed.'}</span>
                <Button variant="secondary" onClick={() => setAttempt((n) => n + 1)}>
                  Reconnect
                </Button>
              </>
            ) : (
              <>
                <AnimatedIcon name="loader-circle" working className="size-5" />
                Connecting
              </>
            )}
          </div>
        )}
      </div>
    </>
  )
}

type TerminalState = { state: 'connecting' | 'ready' | 'exited' | 'closed' | 'error'; message?: string }

/** A shell (`/terminal/ws`): in a container, or an SSH login to a VM. */
export function TerminalDialog({ target, close }: { target: ConsoleTarget | null; close: () => void }) {
  const [status, setStatus] = useState<TerminalState>({ state: 'connecting' })
  const [attempt, setAttempt] = useState(0)
  const tone = status.state === 'ready' ? 'done' : status.state === 'error' ? 'failed' : status.state === 'connecting' ? 'working' : 'neutral'
  return (
    <ConsoleDialog
      open={!!target}
      close={close}
      title={target ? `${target.name} · Terminal` : ''}
      actions={
        <>
          {status.state !== 'ready' && status.state !== 'connecting' && (
            <Button size="sm" variant="secondary" onClick={() => setAttempt((n) => n + 1)}>
              New session
            </Button>
          )}
          <StateTag tone={tone} busy={status.state === 'connecting'} title={status.message}>
            {status.state === 'ready' ? 'Connected' : status.state === 'connecting' ? 'Connecting' : status.state === 'exited' ? 'Exited' : status.state === 'error' ? 'Failed' : 'Closed'}
          </StateTag>
        </>
      }
    >
      {status.state === 'error' && <Notice status="error">{status.message}</Notice>}
      {target && <Terminal key={`${target.id}:${attempt}`} id={target.id} setStatus={setStatus} />}
    </ConsoleDialog>
  )
}

function Terminal({ id, setStatus }: { id: string; setStatus: (status: TerminalState) => void }) {
  const host = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!host.current) return
    let disposed = false
    let cleanup = () => {}
    // xterm.js needs the DOM; load it in the browser only.
    void Promise.all([import('@xterm/xterm'), import('@xterm/addon-fit')]).then(([{ Terminal }, { FitAddon }]) => {
      if (disposed || !host.current) return
      const term = new Terminal({ cursorBlink: true, fontFamily: "'JetBrains Mono Variable', ui-monospace, Menlo, monospace", fontSize: 13, scrollback: 5000 })
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
  }, [id, setStatus])

  return <div className="min-h-60 flex-1 overflow-hidden rounded-lg border border-line bg-black p-2" ref={host} />
}
