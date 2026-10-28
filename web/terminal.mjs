// Web UI terminal relay: one WebSocket per terminal, bridged to the daemon's
// `terminal.open` (an interactive shell in the container or machine). Terminal output goes out as binary messages, events
// (ready, exit, error) as JSON text; the page sends JSON text messages
// `{"input":"..."}` and `{"resize":{"rows":R,"cols":C}}`.
import { stream } from './daemon.mjs'
import { FrameReader, RecordReader, acceptKey, frame } from './desktop.mjs'

const NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/

function dimension(value, fallback) {
  const n = Number(value)
  return Number.isInteger(n) && n >= 1 && n <= 1000 ? n : fallback
}

/** Handle `GET /terminal/ws?id=NAME&rows=R&cols=C` after admission. */
export function upgradeTerminal(req, socket, { origins, daemon }) {
  const url = new URL(req.url, 'http://127.0.0.1')
  const name = url.searchParams.get('id') ?? ''
  const key = req.headers['sec-websocket-key']
  const reject = (status, message) => {
    socket.end(`HTTP/1.1 ${status} ${message}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n`)
  }
  if (url.pathname !== '/terminal/ws') return reject(404, 'Not Found')
  if (!origins.has(String(req.headers.origin ?? ''))) return reject(403, 'Forbidden')
  if (String(req.headers.upgrade ?? '').toLowerCase() !== 'websocket' || !key || req.headers['sec-websocket-version'] !== '13')
    return reject(400, 'Bad Request')
  if (!NAME.test(name)) return reject(400, 'Bad Request')
  const rows = dimension(url.searchParams.get('rows'), 24)
  const cols = dimension(url.searchParams.get('cols'), 80)
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n' +
      `sec-websocket-accept: ${acceptKey(key)}\r\n\r\n`,
  )
  socket.setNoDelay(true)
  const session = stream('terminal.open', { name, rows, cols }, daemon)
  const records = new RecordReader()
  const frames = new FrameReader()
  let closed = false
  const close = () => {
    if (closed) return
    closed = true
    // The daemon hangs the session up.
    session.cancel()
    if (!socket.destroyed) socket.end(frame(0x8, Buffer.alloc(0)))
  }
  session.on('stdout', (chunk) => {
    let list
    try {
      list = records.push(chunk)
    } catch (error) {
      socket.write(frame(0x1, Buffer.from(JSON.stringify({ event: 'error', message: String(error) }))))
      return close()
    }
    for (const { kind, payload } of list) {
      if (socket.destroyed) return
      socket.write(frame(kind === 1 ? 0x1 : 0x2, payload))
    }
  })
  session.on('error', (error) => {
    if (!closed && !socket.destroyed) socket.write(frame(0x1, Buffer.from(JSON.stringify({ event: 'error', message: error.message }))))
    close()
  })
  session.on('end', close)
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
      let message
      try {
        message = JSON.parse(payload.toString('utf8'))
      } catch {
        continue
      }
      session.control(message)
    }
  })
  socket.on('close', close)
  socket.on('error', close)
}
