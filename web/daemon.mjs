// Server-only API 1 client (docs/api.md): the daemon's socket, `hello`, one
// call per connection, and streams of `u8 channel, u32 big-endian length,
// payload` frames. The same wire vectors as crates/api test it.
import { connect } from 'node:net'
import { EventEmitter } from 'node:events'
import { homedir } from 'node:os'
import { join } from 'node:path'

export const API = 1
export const FRAME_MAX = 1 << 20
export const Channel = { stdin: 0, stdout: 1, stderr: 2, event: 3, end: 4, control: 5 }
const CLIENT = 'veynodevm-web/1'

/** `<home>/state/control.sock` of `VEYNODEVM_HOME` (what `veynodevm ui` sets), else the default home. */
export function socketPath(env = process.env) {
  const home = env.VEYNODEVM_HOME || join(homedir(), 'Library/Application Support/VeynodeVM')
  return join(home, 'state', 'control.sock')
}

/** A daemon refusal: its message and its code (`invalid`, `not_found`, `state`, `unsupported`, `version`, `failed`). */
export class ApiError extends Error {
  constructor(message, code) {
    super(message)
    this.code = code ?? 'failed'
  }
}

/** What a client says when the daemon running is not the one it was built with. */
export const RESTART = 'restart the daemon: `veynodevm daemon stop`, then `veynodevm daemon start`'

/** A refused `hello` is a daemon of another API: one older than API 1 refuses the method itself, without a code. */
function refusedHello(error, path) {
  if (!(error instanceof ApiError)) return error
  if (error.code === 'version') return new ApiError(`${error.message}; ${RESTART}`, 'version')
  return new ApiError(`the daemon at ${path} does not speak API ${API} (it answered hello with: ${error.message}); ${RESTART}`, 'version')
}

/** A method the daemon does not know is most often a daemon older than this UI, left running across an upgrade. */
function unknownMethod(error, method, hello) {
  if (!(error instanceof ApiError) || error.message !== `unknown method ${method}`) return error
  return new ApiError(`${error.message}: the daemon is version ${hello?.daemon ?? 'unknown'}; ${RESTART}`, error.code)
}

/** The hello answer, or the refusal explained. */
function openHello(line, path) {
  try {
    return open(line)
  } catch (error) {
    throw refusedHello(error, path)
  }
}

export function encode(channel, payload) {
  const body = Buffer.from(payload)
  if (body.length > FRAME_MAX) throw new Error('frame exceeds 1 MiB')
  const header = Buffer.alloc(5)
  header[0] = channel
  header.writeUInt32BE(body.length, 1)
  return Buffer.concat([header, body])
}

/** The answer envelope as a value, or its refusal thrown. */
export function open(envelope) {
  if (envelope && typeof envelope === 'object' && 'error' in envelope)
    throw new ApiError(String(envelope.error), envelope.code)
  return envelope?.result
}

/** Lines first (hello's answer, the request's), then frames once a stream starts. */
export class WireReader {
  constructor() {
    this.buffer = Buffer.alloc(0)
  }
  push(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk])
  }
  line() {
    const end = this.buffer.indexOf(0x0a)
    if (end < 0) return undefined
    const text = this.buffer.subarray(0, end).toString('utf8')
    this.buffer = this.buffer.subarray(end + 1)
    return JSON.parse(text)
  }
  frame() {
    if (this.buffer.length < 5) return undefined
    const size = this.buffer.readUInt32BE(1)
    if (size > FRAME_MAX) throw new Error('frame exceeds 1 MiB')
    if (this.buffer.length < 5 + size) return undefined
    const channel = this.buffer[0]
    const payload = this.buffer.subarray(5, 5 + size)
    this.buffer = this.buffer.subarray(5 + size)
    return { channel, payload }
  }
}

function request(method, args) {
  return (
    JSON.stringify({ method: 'hello', arguments: { api: API, client: CLIENT } }) +
    '\n' +
    JSON.stringify({ method, arguments: args ?? {} }) +
    '\n'
  )
}

/** A method that answers once. */
export function call(method, args = {}, { timeoutMs = 60_000, path = socketPath() } = {}) {
  return new Promise((resolve, reject) => {
    const socket = connect(path)
    const reader = new WireReader()
    let hello
    const fail = (error) => {
      socket.destroy()
      reject(error)
    }
    const timer = setTimeout(() => fail(new Error(`${method} timed out`)), timeoutMs)
    socket.on('connect', () => socket.write(request(method, args)))
    socket.on('data', (chunk) => {
      reader.push(chunk)
      try {
        let line
        while ((line = reader.line()) !== undefined) {
          if (!hello) {
            hello = openHello(line, path)
            continue
          }
          clearTimeout(timer)
          socket.end()
          try {
            return resolve(open(line))
          } catch (error) {
            throw unknownMethod(error, method, hello)
          }
        }
      } catch (error) {
        clearTimeout(timer)
        fail(error)
      }
    })
    socket.on('error', (error) => {
      clearTimeout(timer)
      reject(error.code === 'ENOENT' || error.code === 'ECONNREFUSED' ? new Error('the daemon is not running') : error)
    })
    socket.on('close', () => {
      clearTimeout(timer)
      reject(new Error(`the daemon closed ${method} before answering`))
    })
  })
}

/**
 * A streaming method as an emitter: `stdout`/`stderr` (Buffers), `event`
 * (values), then `end` (its result) or `error`, and always `close`. Write
 * input with `stdin()`, `closeStdin()` and `control()`; `cancel()` ends it.
 */
export function stream(method, args = {}, { path = socketPath() } = {}) {
  const emitter = new EventEmitter()
  const socket = connect(path)
  const reader = new WireReader()
  let stage = 'hello'
  let hello
  let done = false
  const finish = (name, value) => {
    if (done) return
    done = true
    emitter.emit(name, value)
    socket.destroy()
  }
  // Written now, so input sent before the connection is up follows it.
  socket.write(request(method, args))
  socket.on('data', (chunk) => {
    reader.push(chunk)
    try {
      while (!done) {
        if (stage !== 'frames') {
          const line = reader.line()
          if (line === undefined) return
          if (stage === 'hello') {
            hello = openHello(line, path)
            stage = 'start'
            continue
          }
          let result
          try {
            result = open(line)
          } catch (error) {
            throw unknownMethod(error, method, hello)
          }
          if (result?.stream === true) stage = 'frames'
          // A method that answers once, called as a stream.
          else return finish('end', result)
          continue
        }
        const next = reader.frame()
        if (!next) return
        const { channel, payload } = next
        if (channel === Channel.stdout) emitter.emit('stdout', payload)
        else if (channel === Channel.stderr) emitter.emit('stderr', payload)
        else if (channel === Channel.event) emitter.emit('event', JSON.parse(payload.toString('utf8')))
        else if (channel === Channel.end) finish('end', open(JSON.parse(payload.toString('utf8'))))
      }
    } catch (error) {
      finish('error', error)
    }
  })
  socket.on('error', (error) => finish('error', error))
  socket.on('close', () => {
    finish('error', new Error(`the daemon closed ${method} before its end`))
    emitter.emit('close')
  })
  const send = (channel, payload) => {
    if (!done && !socket.destroyed) socket.write(encode(channel, payload))
  }
  emitter.stdin = (data) => {
    for (let at = 0; at < data.length; at += FRAME_MAX) send(Channel.stdin, data.subarray(at, at + FRAME_MAX))
  }
  emitter.closeStdin = () => send(Channel.stdin, Buffer.alloc(0))
  emitter.control = (message) => send(Channel.control, JSON.stringify(message))
  emitter.cancel = () => {
    done = true
    socket.destroy()
  }
  return emitter
}
