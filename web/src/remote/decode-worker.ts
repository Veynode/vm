// Remote desktop decoding off the page's main thread: video_v2 payloads in,
// WebCodecs, painted onto an OffscreenCanvas the page transferred here with
// `transferControlToOffscreen()`. Start it with
// `new Worker(new URL('./decode-worker.ts', import.meta.url), { type: 'module' })`.
//
// Ported from veynode/core `packages/console/src/lib/desktop-video-decoder.ts`
// (VideoContext: the 100 ms no-picture deadline, retrying H.264 once with
// `prefer-software`, giving up HEVC when it never produced a picture) and
// `packages/console/src/lib/desktop-view-worker.ts` (the worker owning the
// canvas, message shapes, a capability probe). Core decodes RFB rectangles
// per area; here there is one full-screen stream, so one decoder. The
// backlog rule (drop a delta once `decodeQueueSize > 2`) and the H.264 SPS
// rewrite come from this repo's `web/src/desktop/client.ts`.

import { codecString } from '../desktop/client'
import { rewriteAccessUnit } from '../desktop/sps'
import { decodeVideoV2, type View as FrameView } from './proto'

export type ToWorker =
  | { type: 'probe' }
  | { type: 'open'; canvas: OffscreenCanvas; codec: 'h264' | 'hevc' }
  /** A kind 8 payload; `receivedAt` is when its last byte arrived (epoch ms). */
  | { type: 'frame'; bytes: ArrayBuffer; receivedAt: number }
  /** The page switched codec after `hevcFailed` (or a new subscribe). */
  | { type: 'codec'; codec: 'h264' | 'hevc' }
  | { type: 'close' }

export interface Painted {
  epoch: number
  frameSeq: number
  inputAckSeq: number
  encodeUs: number
  hostQueueUs: number
  /** The source area this frame shows; the page maps input through it (view.ts). */
  view: FrameView
  /** Epoch milliseconds (worker `timeOrigin + now`). */
  receivedAt: number
  decodedAt: number
  paintedAt: number
}

export type FromWorker =
  | { type: 'caps'; h264: boolean; hevc: boolean; offscreen: boolean }
  /** Decoding must restart from a key frame: send `keyframe_req`. */
  | { type: 'need_key'; reason: string }
  /** HEVC broke before giving any picture: resubscribe with H.264. */
  | { type: 'hevcFailed' }
  | { type: 'size'; width: number; height: number }
  | ({ type: 'painted' } & Painted)
  | { type: 'error'; message: string }

/** Core's deadline: a picture must come out within this of its submission. */
export const NO_PICTURE_MS = 100

// The project's lib is DOM, not WebWorker; this is the slice of the worker
// scope used here.
interface Scope {
  postMessage(message: FromWorker): void
  onmessage: ((event: MessageEvent<ToWorker>) => void) | null
}
const scope = globalThis as unknown as Scope
const wall = () => performance.timeOrigin + performance.now()

async function capabilities() {
  const offscreen = typeof OffscreenCanvas !== 'undefined'
  const probe = async (codec: string) => {
    try {
      return (await VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true })).supported === true
    } catch {
      return false
    }
  }
  if (typeof VideoDecoder === 'undefined') return { h264: false, hevc: false, offscreen }
  // Constrained Baseline 4.0 and Main 4.1, what the host encoders produce.
  const [h264, hevc] = await Promise.all([probe('avc1.42c028'), probe('hvc1.1.6.L123.B0')])
  return { h264, hevc: h264 && hevc, offscreen }
}

interface Waiting {
  timestamp: number
  submitted: number
  meta: Omit<Painted, 'decodedAt' | 'paintedAt'>
}

class View {
  private readonly ctx: OffscreenCanvasRenderingContext2D
  private decoder: VideoDecoder | undefined
  private configured: string | undefined
  private acceleration: HardwareAcceleration = 'prefer-hardware'
  private producedPicture = false
  private hevcGivenUp = false
  private waitKey = true
  private waiting: Waiting[] = []
  private clock = 0
  private deadline: ReturnType<typeof setTimeout> | undefined
  private readonly spsCache = new Map<string, Uint8Array | null>()
  private readonly canvas: OffscreenCanvas
  codec: 'h264' | 'hevc'

  constructor(canvas: OffscreenCanvas, codec: 'h264' | 'hevc') {
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true })
    if (!ctx) throw new Error('OffscreenCanvas 2D context unavailable')
    this.canvas = canvas
    this.ctx = ctx
    this.codec = codec
  }

  frame(bytes: ArrayBuffer, receivedAt: number) {
    const v = decodeVideoV2(new Uint8Array(bytes))
    const au = v.key && this.codec === 'h264' ? rewriteAccessUnit(v.au, this.spsCache) : v.au
    if (v.key) {
      const codec = codecString(this.codec, au)
      if (!codec) return this.lost('key frame without a parameter set')
      // A new SPS (resize, codec change) reconfigures; same codec keeps the decoder.
      if (codec !== this.configured || this.decoder?.state !== 'configured') this.configure(codec)
    }
    const decoder = this.decoder
    if (!decoder || decoder.state !== 'configured') return this.lost('no decoder before a key frame')
    // Never let the decoder queue grow: a backlog becomes visible latency.
    if (!v.key && decoder.decodeQueueSize > 2) return this.lost('decoder backlog')
    if (this.waitKey && !v.key) return
    this.waitKey = false
    const entry: Waiting = {
      timestamp: ++this.clock,
      submitted: performance.now(),
      meta: { epoch: v.epoch, frameSeq: v.frameSeq, inputAckSeq: v.inputAckSeq, encodeUs: v.encodeUs, hostQueueUs: v.hostQueueUs, view: v.view, receivedAt },
    }
    this.waiting.push(entry)
    this.armDeadline()
    try {
      decoder.decode(new EncodedVideoChunk({ type: v.key ? 'key' : 'delta', timestamp: entry.timestamp, data: au as Uint8Array<ArrayBuffer> }))
    } catch (error) {
      this.reset()
      this.lost(`decode: ${error}`)
    }
  }

  private configure(codec: string) {
    this.reset()
    const decoder = new VideoDecoder({
      output: (frame) => {
        if (this.decoder !== decoder) return frame.close()
        this.output(frame)
      },
      error: (error) => {
        if (this.decoder !== decoder) return
        this.failedEarly()
        this.reset()
        this.lost(`decoder: ${error.message}`)
      },
    })
    decoder.configure({ codec, optimizeForLatency: true, hardwareAcceleration: this.acceleration })
    this.decoder = decoder
    this.configured = codec
  }

  private output(frame: VideoFrame) {
    const decodedAt = wall()
    this.producedPicture = true
    // Frames come out in order; anything older than this one gave no picture.
    let entry = this.waiting.shift()
    while (entry && entry.timestamp < frame.timestamp) entry = this.waiting.shift()
    this.armDeadline()
    if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
      this.canvas.width = frame.displayWidth
      this.canvas.height = frame.displayHeight
      scope.postMessage({ type: 'size', width: frame.displayWidth, height: frame.displayHeight })
    }
    this.ctx.drawImage(frame, 0, 0)
    frame.close()
    if (entry) scope.postMessage({ type: 'painted', ...entry.meta, decodedAt, paintedAt: wall() })
  }

  private armDeadline() {
    clearTimeout(this.deadline)
    const first = this.waiting[0]
    if (!first) return
    this.deadline = setTimeout(
      () => {
        // Older encoders can omit the no-reordering VUI; a hardware decoder
        // then holds pictures this low-latency queue cannot wait for. Retry
        // once in software instead of resetting forever.
        this.failedEarly()
        this.reset()
        this.lost(`no picture within ${NO_PICTURE_MS} ms`)
      },
      Math.max(0, NO_PICTURE_MS - (performance.now() - first.submitted)),
    )
  }

  /** A decoder that broke before any picture: H.264 goes to software once, HEVC is given up. */
  private failedEarly() {
    if (this.producedPicture) return
    if (this.configured?.startsWith('hvc') || this.configured?.startsWith('hev')) {
      if (!this.hevcGivenUp) {
        this.hevcGivenUp = true
        scope.postMessage({ type: 'hevcFailed' })
      }
    } else this.acceleration = 'prefer-software'
  }

  private lost(reason: string) {
    this.waitKey = true
    scope.postMessage({ type: 'need_key', reason })
  }

  private reset() {
    clearTimeout(this.deadline)
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close()
    this.decoder = undefined
    this.configured = undefined
    this.waiting = []
  }

  close() {
    this.reset()
  }
}

let view: View | undefined

scope.onmessage = (event) => {
  const message = event.data
  try {
    switch (message.type) {
      case 'probe':
        void capabilities().then((caps) => scope.postMessage({ type: 'caps', ...caps }))
        return
      case 'open':
        view?.close()
        view = new View(message.canvas, message.codec)
        return
      case 'frame':
        view?.frame(message.bytes, message.receivedAt)
        return
      case 'codec':
        if (view) view.codec = message.codec
        return
      case 'close':
        view?.close()
        view = undefined
        return
    }
  } catch (error) {
    scope.postMessage({ type: 'error', message: error instanceof Error ? error.message : String(error) })
  }
}
