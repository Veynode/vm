import { useEffect, useRef, useState, type ReactNode } from 'react'
import '@xterm/xterm/css/xterm.css'
import { AnimatedIcon } from '@ui/boardui/icons/animated-icon'
import { Button } from '@ui/components/ui/button'
import { CloseButton } from '@ui/components/ui/close-button'
import { Dialog, DialogClose, DialogContent, DialogHeader, DialogTitle } from '@ui/components/ui/dialog'
import { IconButton } from '@ui/components/ui/icon-button'
import { MenuDivider, MenuGroup, MenuRow } from '@ui/components/ui/menu-rows'
import { Popover, PopoverContent, PopoverTrigger } from '@ui/components/ui/popover'
import { Switch } from '@ui/components/ui/switch'
import { Check, Clipboard, Film, Maximize, Minimize } from '@ui/icons'
import { Textarea } from '@ui/components/ui/textarea'
import { InfoTip } from '@ui/settings/info-tip'
import { RowMenu } from '@ui/settings/row-menu'
import { StateTag } from '@ui/settings/state-tag'
import { classes } from '@ui/settings/recipes'
import { outputSelectable } from './desktop/audio'
import { DesktopClient, type DesktopStats } from './desktop/client'
import { DesktopDockPanels, DockPanelButton, useDockHold } from './desktop-dock'
import { useToast } from './ui'

/** A container or VM to open: the id the stream and terminal sockets take, and its name for the title. */
export type ConsoleTarget = { id: string; name: string }

/**
 * A wide dialog over the page it was opened from, framed as core's computer
 * window (`Modal variant="window"`): a 56px head with the title in title-3,
 * then the screen, dark in either theme, a 12px step in. Escape and the scrim
 * do not close it: Escape belongs to the guest (vi, a dialog in Windows), and
 * a stray click outside should not end a session. Its X does.
 */
function ConsoleDialog({
  open,
  close,
  title,
  aside,
  actions,
  windowRef,
  children,
}: {
  open: boolean
  close: () => void
  title: string
  aside?: ReactNode
  actions?: ReactNode
  windowRef?: React.Ref<HTMLDivElement>
  children: ReactNode
}) {
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
      <DialogContent ref={windowRef} className={`${classes('modal wide')} console-window`}>
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
  const client = useRef<DesktopClient | null>(null)
  const frame = useRef<HTMLDivElement>(null)
  const toast = useToast()
  // A notice is said once, when it arrives (core's `DesktopViewerControls`).
  const notice = stats?.notice
  useEffect(() => {
    if (notice) toast(notice.text, notice.variant)
  }, [notice?.id])
  // Fullscreen is the whole window, toolbar and all, as in core: the sound,
  // microphone and clipboard stay at hand.
  const [full, setFull] = useState(false)
  useEffect(() => {
    const change = () => setFull(!!document.fullscreenElement && document.fullscreenElement === frame.current)
    document.addEventListener('fullscreenchange', change)
    return () => document.removeEventListener('fullscreenchange', change)
  }, [])
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
      windowRef={frame}
      actions={
        <>
          <DesktopDockPanels>
            {stats?.audio && <SoundControl client={client} audio={stats.audio} />}
            {stats?.audio && <MicrophoneControl client={client} audio={stats.audio} />}
            {stats?.clipboard && <ClipboardControl client={client} clipboard={stats.clipboard} />}
          </DesktopDockPanels>
          <IconButton
            size="sm"
            variant="ghost"
            icon={full ? Minimize : Maximize}
            aria-label={full ? 'Exit fullscreen' : 'Fullscreen'}
            title={full ? 'Exit fullscreen' : 'Fullscreen'}
            onClick={() => {
              if (full) void document.exitFullscreen()
              else void frame.current?.requestFullscreen().then(() => canvas.current?.focus())
            }}
          />
          <RowMenu label="Stream" items={[{ label: codec === 'hevc' ? 'Use H.264' : 'Use HEVC', icon: Film, onSelect: () => setCodec(codec === 'hevc' ? 'h264' : 'hevc') }]} />
        </>
      }
    >
      {target && <Desktop id={target.id} name={target.name} codec={codec} canvas={canvas} client={client} stats={stats} setStats={setStats} />}
    </ConsoleDialog>
  )
}

type ClientRef = React.RefObject<DesktopClient | null>

/**
 * The audio devices of one kind, read again each time a panel shows them and
 * when one is plugged in or out. A browser names them only to a page that
 * may use the microphone; until then they are numbered.
 */
function useAudioDevices(kind: 'audioinput' | 'audiooutput', asked: number) {
  const [devices, setDevices] = useState<MediaDeviceInfo[]>([])
  useEffect(() => {
    const media = navigator.mediaDevices
    if (!media?.enumerateDevices) return
    let live = true
    const read = () =>
      void media
        .enumerateDevices()
        .then((all) => live && setDevices(all.filter((d) => d.kind === kind && d.deviceId && d.deviceId !== 'default' && d.deviceId !== 'communications')))
        .catch(() => {})
    read()
    media.addEventListener('devicechange', read)
    return () => {
      live = false
      media.removeEventListener('devicechange', read)
    }
  }, [kind, asked])
  return devices
}

/** Asks for the microphone once, only to learn the devices' names, and lets it go at once. */
async function revealAudioDevices() {
  try {
    const media = await navigator.mediaDevices.getUserMedia({ audio: true })
    media.getTracks().forEach((track) => track.stop())
  } catch {
    // Refused: the list stays numbered, with the default still there.
  }
}

/** One kind of device to choose: the system's default first, then each by name. */
function DeviceChoice({ kind, label, chosen, choose, numbered }: { kind: 'audioinput' | 'audiooutput'; label: string; chosen: string; choose: (deviceId: string) => void; numbered: string }) {
  const [asked, setAsked] = useState(0)
  const devices = useAudioDevices(kind, asked)
  // A device that is gone is played as the default, so it reads as one.
  const current = devices.some((d) => d.deviceId === chosen) ? chosen : ''
  const row = (deviceId: string, name: string) => (
    <MenuRow key={deviceId || 'default'} selected={current === deviceId} onClick={() => choose(deviceId)}>
      <Check aria-hidden className={current === deviceId ? '' : 'invisible'} />
      <span className="min-w-0 truncate">{name}</span>
    </MenuRow>
  )
  const unnamed = devices.length > 0 && devices.every((d) => !d.label)
  return (
    <MenuGroup label={label}>
      {row('', 'System default')}
      {devices.map((d, i) => row(d.deviceId, d.label || `${numbered} ${i + 1}`))}
      {unnamed && (
        <MenuRow onClick={() => void revealAudioDevices().then(() => setAsked((n) => n + 1))}>
          <span className="pl-7 text-ink-2">Show device names</span>
        </MenuRow>
      )}
    </MenuGroup>
  )
}

/**
 * The Guest's sound (core's `DesktopSoundControl`): one click turns it on or
 * off. Its panel holds the volume and, where the browser can choose one, the
 * speakers it plays on. Arrow keys on the button change the volume.
 */
function SoundControl({ client, audio }: { client: ClientRef; audio: NonNullable<DesktopStats['audio']> }) {
  const { muted, volume } = audio
  const sound = !muted
  // What turning sound on comes back to after the volume was taken to 0.
  const audible = useRef(volume || 1)
  if (volume > 0) audible.current = volume
  const change = (value: number) => {
    const next = Math.round(Math.max(0, Math.min(1, value)) * 100) / 100
    const c = client.current
    // Turning the volume up is asking for sound.
    if (next > 0 && !sound) c?.audio.setMuted(false)
    c?.audio.setVolume(next)
    c?.audio.resume()
  }
  const toggle = () => {
    const c = client.current
    if (!c) return
    if (sound) return c.audio.setMuted(true)
    if (volume === 0) c.audio.setVolume(audible.current)
    c.audio.setMuted(false)
    c.audio.resume()
  }
  const shown = sound ? volume : 0
  return (
    <DockPanelButton
      animate={{ name: 'volume-2', on: !sound || volume === 0, low: volume < 0.5 }}
      label={sound ? 'Sound off' : 'Sound on'}
      pressed={sound}
      onPress={toggle}
      keyShortcuts="ArrowUp ArrowDown"
      onKey={(event) => {
        const step = event.key === 'ArrowUp' || event.key === 'ArrowRight' ? 0.1 : event.key === 'ArrowDown' || event.key === 'ArrowLeft' ? -0.1 : 0
        if (step) change(shown + step)
        return step !== 0
      }}
      panelLabel="Sound"
    >
      <label className="flex flex-col gap-2 px-2.5 pt-1.5 pb-2">
        <span className="flex items-center justify-between text-caption-1-medium text-ink-2">
          Volume
          <span className="tabular-nums">{Math.round(shown * 100)}%</span>
        </span>
        {/* core's react-aria Slider, drawn on a range input: a 4px track, its fill, a 16px thumb. */}
        <input
          type="range"
          min={0}
          max={100}
          step={1}
          value={Math.round(shown * 100)}
          style={{ '--fill': `${Math.round(shown * 100)}%` } as React.CSSProperties}
          onChange={(e) => change(Number(e.target.value) / 100)}
          className="volume-slider my-2"
        />
      </label>
      {outputSelectable() && (
        <>
          <MenuDivider />
          <DeviceChoice kind="audiooutput" label="Play on" chosen={audio.speaker} choose={(id) => void client.current?.audio.setSpeaker(id)} numbered="Speaker" />
        </>
      )}
    </DockPanelButton>
  )
}

/**
 * This computer's microphone (core's `DesktopMicrophoneControl`): off until
 * clicked, a red dot while the Guest hears it, off again when the viewer
 * closes. Its panel picks which microphone, now or for the next time it is on.
 */
function MicrophoneControl({ client, audio }: { client: ClientRef; audio: NonNullable<DesktopStats['audio']> }) {
  const on = audio.mic === 'on' || audio.mic === 'starting'
  return (
    <span className="relative flex">
      <DockPanelButton
        animate={{ name: 'mic', on: !on }}
        label={on ? 'Microphone off' : 'Microphone on'}
        pressed={on}
        onPress={() => void client.current?.audio.setMic(!on)}
        panelLabel="Choose a microphone"
      >
        <DeviceChoice kind="audioinput" label="Microphone" chosen={audio.microphone} choose={(id) => void client.current?.audio.setMicrophone(id)} numbered="Microphone" />
      </DockPanelButton>
      {audio.mic === 'on' && <span aria-hidden className="pointer-events-none absolute top-1 right-1 size-2 rounded-full bg-state-failed ring-2 ring-surface" />}
    </span>
  )
}

/**
 * The clipboard's button and its panel (core's `ClipboardButton`): sync on is
 * a pressed fill, and the panel holds the sync switch, Send and Get. A dot
 * says the Guest copied text the browser would not let the page write by
 * itself; Get copies it. The panel goes into the fullscreen window while
 * there is one, since only that is drawn.
 */
function ClipboardControl({ client, clipboard }: { client: ClientRef; clipboard: NonNullable<DesktopStats['clipboard']> }) {
  const trigger = useRef<HTMLButtonElement>(null)
  const [open, setOpen] = useState(false)
  const [container, setContainer] = useState<HTMLElement | undefined>()
  const [draft, setDraft] = useState('')
  // The sound's and microphone's hover panel stays shut while this one is open.
  useDockHold(open)
  const syncing = clipboard.mode === 'auto'
  const label = syncing ? 'Clipboard, syncing' : 'Clipboard'
  return (
    <span className="relative inline-flex">
      <Popover
        open={open}
        onOpenChange={(next) => {
          if (next) {
            const full = document.fullscreenElement
            setContainer(full instanceof HTMLElement && full.contains(trigger.current) ? full : undefined)
          }
          setOpen(next)
        }}
      >
        <PopoverTrigger
          ref={trigger}
          render={
            <IconButton
              size="sm"
              variant="ghost"
              className={syncing ? 'bg-surface-2 text-ink' : undefined}
              icon={Clipboard}
              aria-label={label}
              title={label}
              onClick={(event) => event.currentTarget.focus()}
            />
          }
        />
        <PopoverContent align="end" collisionPadding={16} {...(container ? { container } : {})} aria-label="Clipboard" className="w-80 overflow-hidden">
          <div className="flex flex-col gap-3 p-3 text-caption-1-regular text-ink-2">
            <div className="flex items-center gap-1">
              <Switch size="compact" checked={syncing} onCheckedChange={(on) => client.current?.setClipboardMode(on ? 'auto' : 'manual')}>
                <span className="text-caption-1-semibold text-ink">Sync clipboard</span>
              </Switch>
              <InfoTip label="About clipboard sync">
                While on, text you copy on either side can be pasted on the other, and ⌘C, ⌘V and ⌘X work as Ctrl in the desktop. Off, text moves only with the box below and Get.
              </InfoTip>
            </div>
            {/* Text pasted or typed here needs no permission to read (core's DesktopClipboard). */}
            <Textarea
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              aria-label="Text to send"
              placeholder="Paste or type text to send to the desktop"
              autoComplete="off"
              className="h-24 resize-none"
            />
            {/* Get runs inside the click: browsers write the clipboard only in a gesture. */}
            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                disabled={!draft}
                onClick={() => {
                  if (client.current?.sendText(draft)) setDraft('')
                }}
              >
                Send to desktop
              </Button>
              <Button size="sm" variant="outline" onClick={() => void client.current?.copyClipboard()}>
                Get from desktop
              </Button>
            </div>
          </div>
        </PopoverContent>
      </Popover>
      {clipboard.pending && <span aria-hidden className="pointer-events-none absolute top-1 right-1 size-2 rounded-full bg-ink ring-2 ring-surface" />}
    </span>
  )
}


const MAX_RETRIES = 8

function Desktop({
  id,
  name,
  codec,
  canvas,
  client,
  stats,
  setStats,
}: {
  id: string
  name: string
  codec: 'hevc' | 'h264'
  canvas: React.RefObject<HTMLCanvasElement | null>
  client: ClientRef
  stats: DesktopStats | null
  setStats: (stats: DesktopStats | null) => void
}) {
  const [attempt, setAttempt] = useState(0)
  // Consecutive automatic reconnects since the last picture.
  const [retries, setRetries] = useState(0)
  // The speaker's settings outlive a reconnect; the microphone starts off again.
  const speaker = useRef({ muted: false, volume: 1 })
  useEffect(() => {
    if (stats?.audio) speaker.current = { muted: stats.audio.muted, volume: stats.audio.volume }
  }, [stats?.audio])

  useEffect(() => {
    if (!canvas.current) return
    setStats(null)
    if (typeof VideoDecoder === 'undefined') {
      setStats({ state: 'error', width: 0, height: 0, fps: 0, kbps: 0, decodeMs: 0, inputMs: null, message: 'This browser has no WebCodecs VideoDecoder.' })
      return
    }
    const c = new DesktopClient(canvas.current, id, codec)
    c.control = true
    c.audio.muted = speaker.current.muted
    c.audio.volume = speaker.current.volume
    client.current = c
    const off = c.onStats(setStats)
    c.connect()
    canvas.current.focus()
    return () => {
      off()
      c.close()
      if (client.current === c) client.current = null
    }
  }, [id, codec, attempt, canvas, client, setStats])

  const live = stats?.state === 'streaming'
  const ended = stats?.state === 'closed' || stats?.state === 'error'
  // A stream that ends (the daemon restarted, the VM rebooted, the network
  // dropped) reconnects by itself, waiting 1, 2, 4 … up to 10 s between tries;
  // after a minute or so of failures it stops and leaves the button.
  const retrying = ended && typeof VideoDecoder !== 'undefined' && retries < MAX_RETRIES
  useEffect(() => {
    if (live) setRetries(0)
  }, [live])
  useEffect(() => {
    if (!retrying) return
    const timer = setTimeout(() => {
      setRetries((n) => n + 1)
      setAttempt((n) => n + 1)
    }, Math.min(1000 * 2 ** retries, 10_000))
    return () => clearTimeout(timer)
  }, [retrying, retries])
  return (
    <>
      <div className="relative flex min-h-0 flex-1 items-center justify-center overflow-hidden rounded-lg border border-line bg-black">
        <canvas ref={canvas} tabIndex={0} aria-label={`${name} desktop`} className="desktop-canvas control max-h-full" />
        {/* Until the first frame, the picture's place says where the stream is. */}
        {!live && (
          <div role="status" className="absolute inset-0 flex flex-col items-center justify-center gap-3 p-6 text-center text-body-regular text-white/70">
            {ended && !retrying ? (
              <>
                <span className="max-w-md">{stats?.message ?? 'The stream closed.'}</span>
                <Button
                  variant="secondary"
                  onClick={() => {
                    setRetries(0)
                    setAttempt((n) => n + 1)
                  }}
                >
                  Reconnect
                </Button>
              </>
            ) : (
              <>
                <AnimatedIcon name="loader-circle" working className="size-5" />
                {retrying || retries > 0 ? 'Reconnecting' : 'Connecting'}
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
  // A dropped connection (not a shell that exited) opens a new session by
  // itself, on the same back-off as the desktop.
  const [retries, setRetries] = useState(0)
  const dropped = status.state === 'closed' || status.state === 'error'
  const retrying = !!target && dropped && retries < MAX_RETRIES
  useEffect(() => {
    if (status.state === 'ready') setRetries(0)
  }, [status.state])
  // Each opening starts afresh, not from how the last one ended.
  useEffect(() => {
    setStatus({ state: 'connecting' })
    setRetries(0)
  }, [target?.id])
  // A failure that does not retry is said once, as a toast; the tag keeps saying Failed.
  const toast = useToast()
  const failed = status.state === 'error' && !retrying ? (status.message ?? 'The terminal failed.') : null
  useEffect(() => {
    if (failed) toast(failed, 'error')
  }, [failed])
  useEffect(() => {
    if (!retrying) return
    const timer = setTimeout(() => {
      setRetries((n) => n + 1)
      setAttempt((n) => n + 1)
    }, Math.min(1000 * 2 ** retries, 10_000))
    return () => clearTimeout(timer)
  }, [retrying, retries])
  const tone = status.state === 'ready' ? 'done' : status.state === 'error' ? 'failed' : status.state === 'connecting' ? 'working' : 'neutral'
  return (
    <ConsoleDialog
      open={!!target}
      close={close}
      title={target ? `${target.name} · Terminal` : ''}
      actions={
        <>
          {status.state !== 'ready' && status.state !== 'connecting' && !retrying && (
            <Button
              size="sm"
              variant="secondary"
              onClick={() => {
                setRetries(0)
                setAttempt((n) => n + 1)
              }}
            >
              New session
            </Button>
          )}
          <StateTag tone={retrying ? 'working' : tone} busy={status.state === 'connecting' || retrying} title={status.message}>
            {status.state === 'ready' ? 'Connected' : retrying || (status.state === 'connecting' && retries > 0) ? 'Reconnecting' : status.state === 'connecting' ? 'Connecting' : status.state === 'exited' ? 'Exited' : status.state === 'error' ? 'Failed' : 'Closed'}
          </StateTag>
        </>
      }
    >
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
