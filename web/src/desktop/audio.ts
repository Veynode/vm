// Guest audio for the browser desktop: speakers and microphone of a VM with
// `audio` (docs/browser-desktop.md). The viewer is where sound plays: a muted
// or closed viewer asks the relay to stop it. The microphone is off until the
// person turns it on, and is off again whenever the viewer closes.
//
// Speakers: binary messages `0xFF, u32 BE rate, u8 channels, s16le PCM`
// (desktop.mjs marks them; a video unit's first byte, the high byte of a
// microsecond timestamp, is never 0xFF) into an AudioWorklet jitter buffer.
// Microphone: getUserMedia → AudioWorklet → 48 kHz mono s16le, 20 ms per
// `{"mic":"<base64>"}` message.

export const AUDIO_MARK = 0xff
export const MIC_RATE = 48000
/** 20 ms at 48 kHz: the most the relay accepts in one message. */
export const MIC_FRAME = 960

export type MicState = 'off' | 'starting' | 'on' | 'denied' | 'unavailable'

/**
 * Jitter buffer of stereo samples. Self-contained (no fields declared outside
 * the constructor, no outside references) because its source text is also the
 * worklet's: `(${PcmRing})`.
 */
export class PcmRing {
  constructor(capacity: number, target: number) {
    this.left = new Float32Array(capacity)
    this.right = new Float32Array(capacity)
    this.start = 0
    this.size = 0
    this.target = target
    this.primed = false
  }
  declare left: Float32Array
  declare right: Float32Array
  declare start: number
  declare size: number
  declare target: number
  declare primed: boolean

  push(left: Float32Array, right: Float32Array) {
    const capacity = this.left.length
    for (let i = 0; i < left.length; i++) {
      if (this.size === capacity) {
        this.start = (this.start + 1) % capacity
        this.size--
      }
      const at = (this.start + this.size) % capacity
      this.left[at] = left[i]
      this.right[at] = right[i]
      this.size++
    }
    // A backlog (a stalled tab, a burst after a network pause) is latency: skip back to the target.
    if (this.size > this.target * 4) {
      const drop = this.size - this.target
      this.start = (this.start + drop) % capacity
      this.size -= drop
    }
  }

  /** Fills both outputs; silence while priming or after running dry, until the target is buffered again. */
  pull(left: Float32Array, right: Float32Array) {
    if (!this.primed && this.size < this.target) {
      left.fill(0)
      right.fill(0)
      return
    }
    this.primed = true
    const capacity = this.left.length
    const count = Math.min(left.length, this.size)
    for (let i = 0; i < count; i++) {
      const at = (this.start + i) % capacity
      left[i] = this.left[at]
      right[i] = this.right[at]
    }
    left.fill(0, count)
    right.fill(0, count)
    this.start = (this.start + count) % capacity
    this.size -= count
    if (count < left.length) this.primed = false
  }
}

/**
 * Downmix-free mono resampler to 48 kHz s16le frames of MIC_FRAME samples.
 * Self-contained for the same reason as PcmRing.
 */
export class MicChunker {
  constructor(rate: number) {
    this.step = rate / 48000
    this.position = 0
    this.previous = 0
    this.frame = new Int16Array(960)
    this.filled = 0
  }
  declare step: number
  declare position: number
  declare previous: number
  declare frame: Int16Array
  declare filled: number

  /** Input samples in -1..1 at the context rate; returns completed frames. */
  push(input: Float32Array): Int16Array[] {
    const frames: Int16Array[] = []
    // position runs over [-1, input.length): -1 is the previous call's last sample.
    while (this.position < input.length - 1) {
      const index = Math.floor(this.position)
      const t = this.position - index
      const a = index < 0 ? this.previous : input[index]
      const b = input[index + 1]
      const value = Math.max(-1, Math.min(1, a + (b - a) * t))
      this.frame[this.filled++] = Math.round(value * 32767)
      if (this.filled === this.frame.length) {
        frames.push(this.frame)
        this.frame = new Int16Array(960)
        this.filled = 0
      }
      this.position += this.step
    }
    if (input.length) {
      this.previous = input[input.length - 1]
      this.position -= input.length
    }
    return frames
  }
}

/** Speaker record body (after the mark) to one Float32Array per output channel. */
export function decodePcm(body: Uint8Array): { rate: number; left: Float32Array; right: Float32Array } | null {
  if (body.length < 7) return null
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength)
  const rate = view.getUint32(0)
  const channels = body[4]
  const bytes = body.length - 5
  if (rate < 8000 || rate > 96000 || (channels !== 1 && channels !== 2) || bytes % (2 * channels)) return null
  const frames = bytes / (2 * channels)
  const left = new Float32Array(frames)
  const right = channels === 1 ? left : new Float32Array(frames)
  for (let i = 0; i < frames; i++) {
    left[i] = view.getInt16(5 + i * 2 * channels, true) / 32768
    if (channels === 2) right[i] = view.getInt16(5 + i * 4 + 2, true) / 32768
  }
  return { rate, left, right }
}

/** Little-endian s16 samples as base64. */
export function pcmBase64(samples: Int16Array): string {
  const bytes = new Uint8Array(samples.length * 2)
  const view = new DataView(bytes.buffer)
  samples.forEach((sample, i) => view.setInt16(i * 2, sample, true))
  let text = ''
  for (const byte of bytes) text += String.fromCharCode(byte)
  return btoa(text)
}

function workletSource() {
  return `const PcmRing = (${PcmRing});
const MicChunker = (${MicChunker});
registerProcessor('veynode-speakers', class extends AudioWorkletProcessor {
  constructor() {
    super();
    // 60 ms ahead; a stall drops back to it rather than adding delay.
    this.ring = new PcmRing(sampleRate * 2, Math.round(sampleRate * 0.06));
    this.port.onmessage = (m) => this.ring.push(m.data.left, m.data.right);
  }
  process(inputs, outputs) {
    const out = outputs[0];
    this.ring.pull(out[0], out[1] || new Float32Array(out[0].length));
    return true;
  }
});
registerProcessor('veynode-microphone', class extends AudioWorkletProcessor {
  constructor() {
    super();
    this.chunker = new MicChunker(sampleRate);
  }
  process(inputs) {
    const input = inputs[0];
    if (input && input.length) {
      const mono = new Float32Array(input[0].length);
      for (const channel of input) for (let i = 0; i < mono.length; i++) mono[i] += channel[i] / input.length;
      for (const frame of this.chunker.push(mono)) this.port.postMessage(frame, [frame.buffer]);
    }
    return true;
  }
});`
}

/** One viewer's speakers and microphone. `send` writes a JSON message to the relay. */
/** Whether this browser can play on a chosen output rather than only the default. */
export function outputSelectable() {
  return typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype
}

export class DesktopAudio {
  private context: AudioContext | null = null
  private speakers: AudioWorkletNode | null = null
  private ready: Promise<void> | null = null
  private stream: MediaStream | null = null
  private micNode: AudioWorkletNode | null = null
  private gain: GainNode | null = null
  private send: (message: object) => void
  private onState: () => void
  available = false
  muted = false
  volume = 1
  mic: MicState = 'off'
  /** The chosen devices' ids; '' is the system's default. */
  speaker = ''
  microphone = ''

  constructor(send: (message: object) => void, onState: () => void) {
    this.send = send
    this.onState = onState
  }

  /** The relay said this VM has audio: ask for it unless muted. */
  start() {
    this.available = true
    if (!this.muted) this.send({ audio: true })
    this.onState()
  }

  private async graph(rate: number) {
    if (this.context && this.context.sampleRate === rate) return this.ready
    await this.context?.close().catch(() => {})
    const context = new AudioContext({ sampleRate: rate, latencyHint: 'interactive' })
    this.context = context
    this.speakers = null
    this.ready = (async () => {
      const url = URL.createObjectURL(new Blob([workletSource()], { type: 'text/javascript' }))
      try {
        await context.audioWorklet.addModule(url)
      } finally {
        URL.revokeObjectURL(url)
      }
      if (this.context !== context) return
      await this.route(context)
      const gain = new GainNode(context, { gain: this.volume })
      const speakers = new AudioWorkletNode(context, 'veynode-speakers', { numberOfInputs: 0, outputChannelCount: [2] })
      speakers.connect(gain).connect(context.destination)
      this.gain = gain
      this.speakers = speakers
    })()
    return this.ready
  }

  /** A speaker message, mark already removed. */
  async play(body: Uint8Array) {
    if (this.muted) return
    const pcm = decodePcm(body)
    if (!pcm) return
    await this.graph(pcm.rate)
    this.speakers?.port.postMessage({ left: pcm.left, right: pcm.right })
  }

  /** Browsers start audio only after a gesture in the page; every click in the viewer is one. */
  resume() {
    if (this.context?.state === 'suspended') void this.context.resume()
  }

  setMuted(muted: boolean) {
    this.muted = muted
    if (this.available) this.send({ audio: !muted })
    this.onState()
  }

  /** Plays on another output, where the browser can choose one (`AudioContext.setSinkId`). */
  async setSpeaker(deviceId: string) {
    this.speaker = deviceId
    if (this.context) await this.route(this.context)
    this.onState()
  }

  private async route(context: AudioContext) {
    const sink = context as AudioContext & { setSinkId?: (id: string) => Promise<void> }
    if (!sink.setSinkId) return
    // A device that is gone plays on the default.
    await sink.setSinkId(this.speaker).catch(() => sink.setSinkId!('').catch(() => {}))
  }

  /** The microphone to use the next time it is on; one that is on now switches. */
  async setMicrophone(deviceId: string) {
    this.microphone = deviceId
    this.onState()
    if (this.mic !== 'on') return
    this.stopMic('off')
    await this.setMic(true)
  }

  setVolume(volume: number) {
    this.volume = Math.max(0, Math.min(1, volume))
    if (this.gain) this.gain.gain.value = this.volume
    this.onState()
  }

  async setMic(on: boolean) {
    if (!on) return this.stopMic('off')
    if (this.mic === 'on' || this.mic === 'starting') return
    this.mic = 'starting'
    this.onState()
    try {
      if (!navigator.mediaDevices?.getUserMedia) throw new DOMException('no microphone API', 'NotFoundError')
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          channelCount: 1,
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
          ...(this.microphone ? { deviceId: { ideal: this.microphone } } : {}),
        },
      })
      if (this.mic !== 'starting') return stream.getTracks().forEach((t) => t.stop())
      await this.graph(this.context?.sampleRate ?? MIC_RATE)
      const context = this.context!
      const node = new AudioWorkletNode(context, 'veynode-microphone', { numberOfOutputs: 0 })
      node.port.onmessage = (m) => this.send({ mic: pcmBase64(m.data) })
      context.createMediaStreamSource(stream).connect(node)
      this.resume()
      this.stream = stream
      this.micNode = node
      // The device going away (unplugged, permission revoked) turns the button off.
      stream.getTracks().forEach((t) => (t.onended = () => this.stopMic('off')))
      this.mic = 'on'
      this.onState()
    } catch (error) {
      const name = (error as DOMException)?.name
      this.stopMic(name === 'NotAllowedError' || name === 'SecurityError' ? 'denied' : 'unavailable')
    }
  }

  private stopMic(state: MicState) {
    this.stream?.getTracks().forEach((t) => t.stop())
    this.micNode?.disconnect()
    this.stream = null
    this.micNode = null
    this.mic = state
    this.onState()
  }

  close() {
    this.stopMic('off')
    void this.context?.close().catch(() => {})
    this.context = null
  }
}
