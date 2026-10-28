// Low-latency browser desktop: WebSocket access units → WebCodecs → canvas.
//
// Latency choices (after veynode/core's desktop viewer): no jitter buffer or
// <video> element, `optimizeForLatency` decoding, frames painted from the
// decoder callback as soon as they exist, and a decoder queue that is never
// allowed to grow (a backlog drops to the next keyframe). Input goes back on
// the same socket as physical key codes and source-pixel pointer positions.

import { AUDIO_MARK, DesktopAudio, type MicState } from './audio'
import { type ClipboardMode, ShortcutMapper, clipboardText, isMac, saveMode, savedMode } from './clipboard'
import { rewriteAccessUnit, unescapeRbsp } from './sps'
import { watchRefresh } from './refresh'

export interface DesktopStats {
  state: 'connecting' | 'streaming' | 'closed' | 'error'
  backend?: string
  codec?: string
  /** 'guest' when the picture already has the guest's cursor, so the page's own is hidden. */
  cursor?: 'guest' | 'local'
  width: number
  height: number
  fps: number
  kbps: number
  decodeMs: number
  /** Median time from a key/button press leaving the page to the next painted frame. */
  inputMs: number | null
  message?: string
  /** What a press or a failure has to say, once: the viewer shows each new `id` as a toast. */
  notice?: DesktopNotice
  /** Guest speakers and microphone; absent when the VM has no audio. */
  audio?: { muted: boolean; volume: number; mic: MicState; speaker: string; microphone: string }
  /**
   * The Guest clipboard; absent when this desktop does not share it. `pending`: the Guest copied
   * text the browser would not let the page write by itself; Get copies it.
   */
  clipboard?: { mode: ClipboardMode; pending: boolean }
}

export type DesktopNotice = { id: number; text: string; variant: 'error' | 'success' }

type Listener = (stats: DesktopStats) => void
type InputEvent =
  | { t: 'move'; x: number; y: number; w: number; h: number }
  | { t: 'button'; button: 'left' | 'middle' | 'right'; down: boolean }
  | { t: 'wheel'; dx: number; dy: number }
  | { t: 'key'; code: string; down: boolean }

const BUTTONS = ['left', 'middle', 'right'] as const

/** NAL units of an Annex B access unit. */
export function nals(au: Uint8Array): Uint8Array[] {
  const starts: number[] = []
  for (let i = 0; i + 3 <= au.length; i++) {
    if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1) {
      starts.push(i + 3)
      i += 2
    }
  }
  return starts.map((s, n) => {
    let end = n + 1 < starts.length ? starts[n + 1] - 3 : au.length
    if (end > s && au[end - 1] === 0) end--
    return au.subarray(s, end)
  })
}

/** The SPS of a keyframe, as text to compare: it changes with the guest's resolution. */
export function parameterSet(codec: string, au: Uint8Array): string | null {
  const sps = nals(au).find((nal) => (codec === 'h264' ? (nal[0] & 0x1f) === 7 : ((nal[0] >> 1) & 0x3f) === 33))
  return sps ? sps.join(',') : null
}

/** WebCodecs codec string from the SPS in a keyframe. */
export function codecString(codec: string, au: Uint8Array): string | null {
  for (const nal of nals(au)) {
    if (codec === 'h264' && (nal[0] & 0x1f) === 7 && nal.length >= 4) {
      const hex = (n: number) => n.toString(16).padStart(2, '0')
      return `avc1.${hex(nal[1])}${hex(nal[2])}${hex(nal[3])}`
    }
    if (codec === 'hevc' && ((nal[0] >> 1) & 0x3f) === 33) {
      // NAL header (2) + vps/max_sub_layers/nesting (1) + profile_tier_level.
      const sps = unescapeRbsp(nal)
      if (sps.length < 15) return null
      const ptl = sps[3]
      const space = ['', 'A', 'B', 'C'][ptl >> 6]
      const tier = ptl & 0x20 ? 'H' : 'L'
      const profile = ptl & 0x1f
      let compat = 0
      for (let i = 0; i < 4; i++) compat = (compat << 8) | sps[4 + i]
      // The compatibility flags are written bit-reversed in the codec string.
      let reversed = 0
      for (let i = 0; i < 32; i++) if (compat & (1 << i)) reversed |= 1 << (31 - i)
      const constraints: string[] = []
      let last = 0
      for (let i = 0; i < 6; i++) if (sps[8 + i]) last = i + 1
      for (let i = 0; i < Math.max(last, 1); i++) constraints.push(sps[8 + i].toString(16).toUpperCase())
      return `hvc1.${space}${profile}.${(reversed >>> 0).toString(16).toUpperCase()}.${tier}${sps[14]}.${constraints.join('.')}`
    }
  }
  return null
}

export class DesktopClient {
  private socket: WebSocket | null = null
  private spsCache = new Map<string, Uint8Array | null>()
  private decoder: VideoDecoder | null = null
  private sps: string | null = null
  private ctx: CanvasRenderingContext2D
  private listeners = new Set<Listener>()
  private stats: DesktopStats = { state: 'connecting', width: 0, height: 0, fps: 0, kbps: 0, decodeMs: 0, inputMs: null }
  private waitingForKey = true
  private frames = 0
  private bytes = 0
  private submitted = new Map<number, number>()
  private decodeTotal = 0
  private decoded = 0
  private timer: ReturnType<typeof setInterval> | null = null
  private pendingMove: InputEvent | null = null
  private wheel = { x: 0, y: 0 }
  private cleanup: (() => void)[] = []
  private pressAt: number | null = null
  private latencies: number[] = []
  control = true
  readonly audio: DesktopAudio
  private clipboardMode: ClipboardMode
  private clipboardAvailable = false
  /** Text both clipboards are known to hold, so a paste does not resend it. */
  private synced: string | null = null
  /** The Guest's latest copy, while the page could not write it (no focus, no permission). */
  private pendingCopy: string | null = null
  private notices = 0
  /** A copy in flight: the Guest's next clipboard event settles it. */
  private copyWaiter: { resolve: (text: string) => void; reject: (error: Error) => void } | null = null
  private shortcuts = new ShortcutMapper(isMac())
  /** Stops measuring this screen's refresh, which caps the encoder (`fps N`). */
  private stopRefresh: (() => void) | null = null

  constructor(
    private canvas: HTMLCanvasElement,
    private name: string,
    private codec: 'hevc' | 'h264',
  ) {
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true })
    if (!ctx) throw new Error('canvas 2D context unavailable')
    this.ctx = ctx
    this.audio = new DesktopAudio(
      (message) => this.socket?.readyState === WebSocket.OPEN && this.socket.send(JSON.stringify(message)),
      () => {
        const { available, muted, volume, mic, speaker, microphone } = this.audio
        if (!available) return
        if (mic !== this.stats.audio?.mic) {
          if (mic === 'denied') this.notify("The browser blocked the microphone. Allow it in this site's settings, then turn it on again.")
          if (mic === 'unavailable') this.notify('No microphone is available to this browser.')
        }
        this.emit({ audio: { muted, volume, mic, speaker, microphone } })
      },
    )
    this.clipboardMode = savedMode(name)
  }

  onStats(listener: Listener) {
    this.listeners.add(listener)
    listener(this.stats)
    return () => this.listeners.delete(listener)
  }

  private notify(text: string, variant: DesktopNotice['variant'] = 'error') {
    this.emit({ notice: { id: ++this.notices, text, variant } })
  }

  private clipboardState() {
    return { clipboard: { mode: this.clipboardMode, pending: this.pendingCopy !== null } }
  }

  private emit(update: Partial<DesktopStats>) {
    this.stats = { ...this.stats, ...update }
    for (const l of this.listeners) l(this.stats)
  }

  connect() {
    const url = new URL('/desktop/ws', location.href)
    url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    url.searchParams.set('id', this.name)
    url.searchParams.set('codec', this.codec)
    const socket = new WebSocket(url)
    socket.binaryType = 'arraybuffer'
    this.socket = socket
    socket.onmessage = (m) => {
      if (typeof m.data === 'string') return this.onEvent(JSON.parse(m.data))
      const data = new Uint8Array(m.data)
      if (data[0] === AUDIO_MARK) void this.audio.play(data.subarray(1))
      else this.onUnit(data)
    }
    socket.onclose = () => {
      if (this.stats.state !== 'error') this.emit({ state: 'closed' })
    }
    this.timer = setInterval(() => {
      this.emit({
        fps: this.frames,
        kbps: Math.round((this.bytes * 8) / 1000),
        decodeMs: this.decoded ? Math.round((this.decodeTotal / this.decoded) * 10) / 10 : 0,
      })
      this.frames = this.bytes = this.decodeTotal = this.decoded = 0
    }, 1000)
    this.attachInput()
  }

  close() {
    if (this.timer) clearInterval(this.timer)
    this.stopRefresh?.()
    for (const c of this.cleanup) c()
    this.cleanup = []
    this.socket?.close()
    this.audio.close()
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close()
  }

  private onEvent(event: any) {
    if (event.event === 'ready') {
      this.canvas.style.cursor = event.cursor === 'guest' ? 'none' : ''
      this.emit({ state: 'connecting', backend: event.backend, codec: event.codec, cursor: event.cursor })
      // Frames past what this screen shows would only split the same bitrate thinner.
      this.stopRefresh?.()
      this.stopRefresh = watchRefresh((hz) => this.message({ encoder: `fps ${hz}` }))
      if (event.audio) this.audio.start()
      // A macOS Guest's shortcuts are ⌘ shortcuts already.
      if (event.guest === 'macos') this.shortcuts = new ShortcutMapper(isMac(), true)
      if (event.clipboard) {
        this.clipboardAvailable = true
        if (this.clipboardMode === 'auto') this.message({ clipboard_watch: true })
        this.emit(this.clipboardState())
      }
    }
    else if (event.event === 'clipboard') this.onClipboard(event)
    else if (event.event === 'error') this.emit({ state: 'error', message: event.message })
    else if (event.event === 'input_error') this.notify(`The desktop did not take the input: ${event.message}`)
  }

  private message(message: object) {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message))
  }

  private onClipboard(event: { text?: string; error?: string }) {
    const waiter = this.copyWaiter
    this.copyWaiter = null
    if (typeof event.text !== 'string') {
      if (waiter) waiter.reject(new Error(event.error ?? 'The desktop clipboard is unavailable.'))
      else this.notify(`Clipboard: ${event.error ?? 'the desktop clipboard is unavailable.'}`)
      return
    }
    this.synced = event.text
    if (waiter) return waiter.resolve(event.text)
    if (this.clipboardMode !== 'auto') return
    // Without a copy in flight, a Guest copy reaches the page while it has focus. Safari needs a
    // gesture and declines (its copies go through the shortcut's ClipboardItem); one that does not
    // land waits for the canvas's next focus or the Get button.
    this.setPending(event.text)
    if (document.hasFocus()) this.flushPending()
  }

  private setPending(text: string | null) {
    if (this.pendingCopy === text) return
    this.pendingCopy = text
    this.emit(this.clipboardState())
  }

  private flushPending() {
    const text = this.pendingCopy
    if (text === null || !navigator.clipboard?.writeText) return
    void navigator.clipboard.writeText(text).then(
      () => this.pendingCopy === text && this.setPending(null),
      () => {},
    )
  }

  /**
   * Writes the Guest's next clipboard text to the page's clipboard. Called inside the key or click
   * that asked for it: the write starts in the gesture and finishes when the text arrives.
   */
  private copyFromGuest(request: boolean) {
    this.copyWaiter?.reject(new Error('superseded'))
    const text = new Promise<string>((resolve, reject) => {
      this.copyWaiter = { resolve, reject }
      setTimeout(() => reject(new Error('the desktop did not copy anything.')), 2000)
    })
    text.catch(() => {})
    if (request) this.message({ clipboard_get: true })
    if (typeof ClipboardItem !== 'undefined' && navigator.clipboard?.write) {
      const blob = text.then((t) => new Blob([t], { type: 'text/plain' }))
      return navigator.clipboard.write([new ClipboardItem({ 'text/plain': blob })])
    }
    return text.then((t) => navigator.clipboard.writeText(t))
  }

  /** Offers the page's text to the Guest; false when there is nothing new to send. */
  private pushToGuest(text: string): boolean {
    const bounded = clipboardText(text)
    if (bounded === null) {
      this.notify('The clipboard is over 200 KiB; it was not sent.')
      return false
    }
    if (!bounded || bounded === this.synced) return false
    this.synced = bounded
    // The page's own copy is newer than what the Guest left waiting.
    this.setPending(null)
    this.message({ clipboard: bounded })
    return true
  }

  setClipboardMode(mode: ClipboardMode) {
    this.clipboardMode = mode
    saveMode(this.name, mode)
    this.shortcuts.reset()
    if (this.clipboardAvailable) this.message({ clipboard_watch: mode === 'auto' })
    this.emit(this.clipboardState())
  }

  /** Toolbar: text the person pasted or typed, to the Guest's clipboard; false when it was not sent. */
  sendText(text: string): boolean {
    this.synced = null
    if (!this.pushToGuest(text)) return false
    this.notify('Sent to the desktop clipboard.', 'success')
    return true
  }

  /** Toolbar: the Guest's clipboard to the page's. */
  async copyClipboard() {
    try {
      // What already waits is written at once: fetched again, it would land outside this click.
      const pending = this.pendingCopy
      if (pending !== null) await navigator.clipboard.writeText(pending)
      else await this.copyFromGuest(true)
      if (pending !== null && this.pendingCopy === pending) this.setPending(null)
      this.notify('Copied the desktop clipboard to this computer.', 'success')
    } catch (error) {
      const refused = error instanceof DOMException && error.name === 'NotAllowedError'
      this.notify(refused ? 'The browser did not allow writing the clipboard.' : `Clipboard: ${(error as Error).message}`)
    }
  }

  private requestKeyframe() {
    this.waitingForKey = true
    this.socket?.readyState === WebSocket.OPEN && this.socket.send('{"keyframe":true}')
  }

  private onUnit(data: Uint8Array) {
    const view = new DataView(data.buffer, data.byteOffset)
    const pts = Number(view.getBigUint64(0))
    const keyframe = data[8] === 1
    // Keyframes carry the SPS; H.264 needs it rewritten for no-reorder output.
    const au = keyframe && this.codec === 'h264' ? rewriteAccessUnit(data.subarray(9), this.spsCache) : data.subarray(9)
    this.bytes += data.length
    // The first keyframe configures the decoder; one with a new SPS (the guest
    // changed resolution, and the encoder started over) configures it again.
    const sps = keyframe ? parameterSet(this.codec, au) : null
    if (keyframe && (!this.decoder || sps !== this.sps)) {
      const codec = codecString(this.codec, au)
      if (!codec || !sps) return this.emit({ state: 'error', message: 'keyframe without a parameter set' })
      this.sps = sps
      this.configure(codec)
    }
    const decoder = this.decoder
    if (!decoder || decoder.state !== 'configured') return
    // Never let the decoder queue grow: a backlog becomes visible latency.
    if (decoder.decodeQueueSize > 2 && !keyframe) return this.requestKeyframe()
    if (this.waitingForKey && !keyframe) return
    this.waitingForKey = false
    this.submitted.set(pts, performance.now())
    try {
      decoder.decode(new EncodedVideoChunk({ type: keyframe ? 'key' : 'delta', timestamp: pts, data: au }))
    } catch (error) {
      this.emit({ state: 'error', message: String(error) })
    }
  }

  private configure(codec: string) {
    const config = { codec, optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware' } as const
    if (this.decoder && this.decoder.state === 'configured') return this.decoder.configure(config)
    const decoder = new VideoDecoder({
      output: (frame) => {
        const started = this.submitted.get(frame.timestamp)
        if (started !== undefined) {
          this.submitted.delete(frame.timestamp)
          this.decodeTotal += performance.now() - started
          this.decoded++
        }
        if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
          this.canvas.width = frame.displayWidth
          this.canvas.height = frame.displayHeight
          this.emit({ width: frame.displayWidth, height: frame.displayHeight })
        }
        this.ctx.drawImage(frame, 0, 0)
        frame.close()
        if (this.pressAt !== null && performance.now() - this.pressAt > 1000) this.pressAt = null
        if (this.pressAt !== null) {
          this.latencies = [...this.latencies.slice(-19), performance.now() - this.pressAt]
          this.pressAt = null
          const sorted = [...this.latencies].sort((a, b) => a - b)
          this.emit({ inputMs: Math.round(sorted[sorted.length >> 1]) })
        }
        this.frames++
        if (this.stats.state !== 'streaming') this.emit({ state: 'streaming' })
        if (this.submitted.size > 32) this.submitted.clear()
      },
      error: (error) => {
        this.emit({ state: 'error', message: `decoder: ${error.message}` })
      },
    })
    decoder.configure(config)
    this.decoder = decoder
  }

  private send(events: InputEvent[]) {
    if (!this.control || !events.length || this.socket?.readyState !== WebSocket.OPEN) return
    this.socket.send(JSON.stringify({ events }))
    // Key presses (typing echo) measure the round trip; a click may start an
    // application, which is guest work rather than transport latency. Only a
    // press with no sample in flight counts, so fast typing is not credited
    // with a frame an earlier key produced.
    // A press that drew nothing (a modifier) expires after a second.
    const now = performance.now()
    if ((this.pressAt === null || now - this.pressAt > 1000) && events.some((e) => e.t === 'key' && e.down)) this.pressAt = now
  }

  private position(e: PointerEvent | WheelEvent): InputEvent | null {
    const { width, height } = this.canvas
    if (!width || !height) return null
    const rect = this.canvas.getBoundingClientRect()
    const x = Math.floor(((e.clientX - rect.left) / rect.width) * width)
    const y = Math.floor(((e.clientY - rect.top) / rect.height) * height)
    if (x < 0 || y < 0 || x >= width || y >= height) return null
    return { t: 'move', x, y, w: width, h: height }
  }

  private attachInput() {
    const c = this.canvas
    const on = <K extends keyof HTMLElementEventMap>(type: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) => {
      c.addEventListener(type, fn as EventListener, opts)
      this.cleanup.push(() => c.removeEventListener(type, fn as EventListener, opts))
    }
    // Pointer moves are coalesced to one per animation frame; buttons, keys
    // and wheel flush the pending move first so ordering is preserved.
    let scheduled = false
    const flushMove = () => {
      scheduled = false
      if (this.pendingMove) this.send([this.pendingMove])
      this.pendingMove = null
    }
    on('pointermove', (e) => {
      const move = this.position(e)
      if (!move) return
      this.pendingMove = move
      if (!scheduled) {
        scheduled = true
        requestAnimationFrame(flushMove)
      }
    })
    on('pointerdown', (e) => {
      c.focus()
      this.audio.resume()
      c.setPointerCapture(e.pointerId)
      const move = this.position(e)
      const button = BUTTONS[e.button]
      this.pendingMove = null
      if (move && button) this.send([move, { t: 'button', button, down: true }])
      e.preventDefault()
    })
    on('pointerup', (e) => {
      const button = BUTTONS[e.button]
      const move = this.position(e)
      this.pendingMove = null
      if (button) this.send([...(move ? [move] : []), { t: 'button', button, down: false }])
    })
    on('contextmenu', (e) => e.preventDefault())
    on(
      'wheel',
      (e) => {
        e.preventDefault()
        const unit = e.deltaMode === 1 ? 1 / 3 : e.deltaMode === 2 ? 1 : 1 / 100
        this.wheel.x += e.deltaX * unit
        this.wheel.y += e.deltaY * unit
        const dx = Math.trunc(this.wheel.x)
        const dy = Math.trunc(this.wheel.y)
        if (!dx && !dy) return
        this.wheel.x -= dx
        this.wheel.y -= dy
        const move = this.position(e)
        this.send([...(move ? [move] : []), { t: 'wheel', dx: Math.max(-10, Math.min(10, dx)), dy: Math.max(-10, Math.min(10, dy)) }])
      },
      { passive: false },
    )
    // A paste shortcut waits for the page's `paste` event to carry its text; a browser that sends
    // none (nothing to paste, or no paste event on a canvas) pastes the Guest's own clipboard.
    let pasting: ReturnType<typeof setTimeout> | null = null
    const paste = (text: string | null) => {
      if (pasting === null) return
      clearTimeout(pasting)
      pasting = null
      const keys = this.shortcuts.pasteKeys()
      // The Guest takes the text before the shortcut asks for it.
      if (text !== null && this.pushToGuest(text)) setTimeout(() => this.send(keys), 50)
      else this.send(keys)
    }
    on('paste', (e) => {
      e.preventDefault()
      paste(e.clipboardData?.getData('text/plain') ?? null)
    })
    const key = (down: boolean) => (e: KeyboardEvent) => {
      if (!this.control) return
      const auto = this.clipboardAvailable && this.clipboardMode === 'auto'
      if (!auto) {
        e.preventDefault()
        if (e.repeat || !e.code) return
        this.send([{ t: 'key', code: e.code, down }])
        return
      }
      if (e.repeat || !e.code) {
        e.preventDefault()
        return
      }
      const action = this.shortcuts.key(e.code, down, e.ctrlKey, e.metaKey)
      if (!action.allowDefault) e.preventDefault()
      if (action.paste) {
        if (pasting !== null) clearTimeout(pasting)
        pasting = setTimeout(() => paste(null), 150)
      }
      if (action.copy) void this.copyFromGuest(false).catch(() => {})
      this.send(action.keys)
    }
    on('keydown', key(true))
    on('keydown', () => this.audio.resume())
    on('keyup', key(false))
    // Losing focus must not leave modifiers pressed in the guest.
    const pressed = new Set<string>()
    on('keydown', (e) => pressed.add(e.code))
    on('keyup', (e) => pressed.delete(e.code))
    on('blur', () => {
      // A ⌘ shortcut held as Ctrl releases the Ctrl the page sent in its place.
      if (this.shortcuts.mapped.size && this.shortcuts.mac) pressed.add('ControlLeft')
      this.send([...pressed].map((code) => ({ t: 'key', code, down: false }) as InputEvent))
      pressed.clear()
      this.shortcuts.reset()
    })
    // Coming back to the desktop brings the page's latest copy along, where the browser lets the
    // page read its clipboard without asking (a paste shortcut sends it anyway).
    on('focus', () => {
      if (!this.clipboardAvailable || this.clipboardMode !== 'auto') return
      this.flushPending()
      void navigator.permissions
        ?.query({ name: 'clipboard-read' as PermissionName })
        .then((p) => (p.state === 'granted' ? navigator.clipboard.readText() : null))
        .then((text) => text !== null && this.pushToGuest(text))
        .catch(() => {})
    })
  }
}
