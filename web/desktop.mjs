// Browser desktop relay: one WebSocket per viewer, bridged to a
// `veynodevm desktop --stream` child. Encoded access units go out as binary
// messages (u64 pts_us, u8 keyframe, Annex B) as soon as they arrive; input
// comes back as JSON text. Minimal RFC 6455 server, no dependency: text and
// binary frames, ping/pong and close; no extensions or fragmentation on send.
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
const MAX_MESSAGE = 64 * 1024
// A viewer that falls this far behind drops delta frames until a keyframe:
// latency stays bounded instead of queueing seconds of video.
const MAX_BUFFERED = 2 * 1024 * 1024
const NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/

export function acceptKey(key) {
  return createHash('sha1').update(key + GUID).digest('base64')
}

export function frame(opcode, payload) {
  const length = payload.length
  const header = length < 126 ? Buffer.alloc(2) : length < 65536 ? Buffer.alloc(4) : Buffer.alloc(10)
  header[0] = 0x80 | opcode
  if (length < 126) header[1] = length
  else if (length < 65536) {
    header[1] = 126
    header.writeUInt16BE(length, 2)
  } else {
    header[1] = 127
    header.writeBigUInt64BE(BigInt(length), 2)
  }
  return Buffer.concat([header, payload])
}

/** Incremental parser for masked client frames. Returns complete messages. */
export class FrameReader {
  constructor() {
    this.buffer = Buffer.alloc(0)
    this.parts = []
    this.opcode = 0
  }
  push(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk])
    const out = []
    for (;;) {
      if (this.buffer.length < 2) break
      const fin = (this.buffer[0] & 0x80) !== 0
      const opcode = this.buffer[0] & 0x0f
      const masked = (this.buffer[1] & 0x80) !== 0
      let length = this.buffer[1] & 0x7f
      let offset = 2
      if (length === 126) {
        if (this.buffer.length < 4) break
        length = this.buffer.readUInt16BE(2)
        offset = 4
      } else if (length === 127) {
        if (this.buffer.length < 10) break
        const big = this.buffer.readBigUInt64BE(2)
        if (big > BigInt(MAX_MESSAGE)) throw new Error('message too large')
        length = Number(big)
        offset = 10
      }
      if (!masked) throw new Error('client frames must be masked')
      if (length > MAX_MESSAGE) throw new Error('message too large')
      if (this.buffer.length < offset + 4 + length) break
      const mask = this.buffer.subarray(offset, offset + 4)
      const payload = Buffer.from(this.buffer.subarray(offset + 4, offset + 4 + length))
      for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i & 3]
      this.buffer = this.buffer.subarray(offset + 4 + length)
      if (opcode >= 0x8) {
        out.push({ opcode, payload })
        continue
      }
      if (opcode !== 0) this.opcode = opcode
      this.parts.push(payload)
      if (this.parts.reduce((n, p) => n + p.length, 0) > MAX_MESSAGE) throw new Error('message too large')
      if (fin) {
        out.push({ opcode: this.opcode, payload: Buffer.concat(this.parts) })
        this.parts = []
      }
    }
    return out
  }
}

/** Split relay stdout into `u32 length, u8 kind, payload` records. */
export class RecordReader {
  constructor() {
    this.buffer = Buffer.alloc(0)
  }
  push(chunk) {
    this.buffer = this.buffer.length ? Buffer.concat([this.buffer, chunk]) : chunk
    const out = []
    while (this.buffer.length >= 5) {
      const length = this.buffer.readUInt32BE(0)
      if (length < 1 || length > 8 * 1024 * 1024) throw new Error('relay record outside bound')
      if (this.buffer.length < 4 + length) break
      out.push({ kind: this.buffer[4], payload: this.buffer.subarray(5, 4 + length) })
      this.buffer = this.buffer.subarray(4 + length)
    }
    return out
  }
}

/**
 * Handle `GET /desktop/ws?id=NAME&codec=hevc|h264` after admission.
 * `origins` are the exact page origins allowed to open the socket.
 */
export function upgradeDesktop(req, socket, { cli, origins }) {
  const url = new URL(req.url, 'http://127.0.0.1')
  const name = url.searchParams.get('id') ?? ''
  const codec = url.searchParams.get('codec') ?? 'hevc'
  const key = req.headers['sec-websocket-key']
  const reject = (status, message) => {
    socket.end(`HTTP/1.1 ${status} ${message}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n`)
  }
  if (url.pathname !== '/desktop/ws') return reject(404, 'Not Found')
  if (!origins.has(String(req.headers.origin ?? ''))) return reject(403, 'Forbidden')
  if (String(req.headers.upgrade ?? '').toLowerCase() !== 'websocket' || !key || req.headers['sec-websocket-version'] !== '13')
    return reject(400, 'Bad Request')
  if (!NAME.test(name) || !['hevc', 'h264'].includes(codec)) return reject(400, 'Bad Request')
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n' +
      `sec-websocket-accept: ${acceptKey(key)}\r\n\r\n`,
  )
  socket.setNoDelay(true)
  const child = spawn(cli, ['desktop', '--stream', '--codec', codec, name], { stdio: ['pipe', 'pipe', 'inherit'] })
  const records = new RecordReader()
  const frames = new FrameReader()
  let waitingForKey = false
  let closed = false
  const close = () => {
    if (closed) return
    closed = true
    child.stdin.end()
    setTimeout(() => child.kill('SIGTERM'), 5000).unref()
    if (!socket.destroyed) socket.end(frame(0x8, Buffer.alloc(0)))
  }
  child.stdout.on('data', (chunk) => {
    let list
    try {
      list = records.push(chunk)
    } catch (error) {
      socket.write(frame(0x1, Buffer.from(JSON.stringify({ event: 'error', message: String(error) }))))
      return close()
    }
    for (const { kind, payload } of list) {
      if (socket.destroyed) return
      if (kind === 1) {
        socket.write(frame(0x1, payload))
        continue
      }
      const keyframe = payload[8] === 1
      if (socket.writableLength > MAX_BUFFERED) {
        if (!waitingForKey) child.stdin.write('{"keyframe":true}\n')
        waitingForKey = true
        continue
      }
      if (waitingForKey && !keyframe) continue
      waitingForKey = false
      socket.write(frame(0x2, payload))
    }
  })
  child.on('exit', close)
  socket.on('data', (chunk) => {
    let messages
    try {
      messages = frames.push(chunk)
    } catch {
      return close()
    }
    for (const { opcode, payload } of messages) {
      if (opcode === 0x8) return close()
      if (opcode === 0x9) socket.write(frame(0xa, payload))
      if (opcode !== 0x1) continue
      const text = payload.toString('utf8')
      if (text.includes('\n')) continue
      child.stdin.write(text + '\n')
    }
  })
  socket.on('close', close)
  socket.on('error', close)
}
