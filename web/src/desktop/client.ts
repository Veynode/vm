// Low-latency browser desktop: WebSocket access units → WebCodecs → canvas.
//
// Latency choices (after veynode/core's desktop viewer): no jitter buffer or
// <video> element, `optimizeForLatency` decoding, frames painted from the
// decoder callback as soon as they exist, and a decoder queue that is never
// allowed to grow (a backlog drops to the next keyframe). Input goes back on
// the same socket as physical key codes and source-pixel pointer positions.

import { rewriteAccessUnit, unescapeRbsp } from './sps'

export interface DesktopStats {
  state: 'connecting' | 'streaming' | 'closed' | 'error'
  backend?: string
  codec?: string
  width: number
  height: number
  fps: number
  kbps: number
  decodeMs: number
  /** Median time from a key/button press leaving the page to the next painted frame. */
  inputMs: number | null
  message?: string
  inputError?: string
}

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

  constructor(
    private canvas: HTMLCanvasElement,
    private name: string,
    private codec: 'hevc' | 'h264',
  ) {
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true })
    if (!ctx) throw new Error('canvas 2D context unavailable')
    this.ctx = ctx
  }

  onStats(listener: Listener) {
    this.listeners.add(listener)
    listener(this.stats)
    return () => this.listeners.delete(listener)
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
    socket.onmessage = (m) => (typeof m.data === 'string' ? this.onEvent(JSON.parse(m.data)) : this.onUnit(new Uint8Array(m.data)))
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
    for (const c of this.cleanup) c()
    this.cleanup = []
    this.socket?.close()
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close()
  }

  private onEvent(event: any) {
    if (event.event === 'ready') this.emit({ state: 'connecting', backend: event.backend, codec: event.codec })
    else if (event.event === 'error') this.emit({ state: 'error', message: event.message })
    else if (event.event === 'input_error') this.emit({ inputError: event.message })
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
    if (keyframe && !this.decoder) {
      const codec = codecString(this.codec, au)
      if (!codec) return this.emit({ state: 'error', message: 'keyframe without a parameter set' })
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
    decoder.configure({ codec, optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware' })
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
    const key = (down: boolean) => (e: KeyboardEvent) => {
      if (!this.control) return
      e.preventDefault()
      if (e.repeat || !e.code) return
      this.send([{ t: 'key', code: e.code, down }])
    }
    on('keydown', key(true))
    on('keyup', key(false))
    // Losing focus must not leave modifiers pressed in the guest.
    const pressed = new Set<string>()
    on('keydown', (e) => pressed.add(e.code))
    on('keyup', (e) => pressed.delete(e.code))
    on('blur', () => {
      this.send([...pressed].map((code) => ({ t: 'key', code, down: false }) as InputEvent))
      pressed.clear()
    })
  }
}
