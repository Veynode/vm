import assert from 'node:assert/strict'
import { readFileSync, mkdtempSync, rmSync } from 'node:fs'
import { createServer } from 'node:net'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { ApiError, Channel, RESTART, WireReader, call, encode, open, stream } from '../daemon.mjs'

const vectors = JSON.parse(readFileSync(new URL('../../crates/api/tests/vectors/wire.json', import.meta.url), 'utf8'))
const hex = (text) => Buffer.from(text.replace(/\s+/g, ''), 'hex')

test('frames match the shared wire vectors', () => {
  for (const { name, channel, payload, wire } of vectors.frames) {
    assert.deepEqual(encode(channel, hex(payload)), hex(wire), name)
    const reader = new WireReader()
    reader.push(hex(wire))
    assert.deepEqual(reader.frame(), { channel, payload: hex(payload) }, name)
  }
})

test('envelopes open as the shared vectors say', () => {
  const line = (name) => vectors.lines.find((c) => c.name === name).value
  assert.deepEqual(open(line('stream start')), { stream: true })
  assert.throws(() => open(line('refusal')), (e) => e instanceof ApiError && e.code === 'not_found')
  assert.throws(() => open(line('version refusal')), (e) => e.code === 'version')
})

/** A daemon that answers hello (with `hello`, by default API 1), then hands the request to `serve`. */
function fakeDaemon(serve, hello = { result: { api: 1, daemon: '0.0.9', methods: [] } }) {
  const dir = mkdtempSync(join(tmpdir(), 'vvm-web-'))
  const path = join(dir, 'control.sock')
  const sockets = new Set()
  const server = createServer((socket) => {
    sockets.add(socket)
    const reader = new WireReader()
    let lines = []
    socket.on('data', (chunk) => {
      reader.push(chunk)
      let line
      while (lines.length < 2 && (line = reader.line()) !== undefined) {
        lines.push(line)
        if (lines.length === 1) socket.write(JSON.stringify(hello) + '\n')
        // A refused hello ends the connection, as the daemon does.
        if (lines.length === 1 && 'error' in hello) return socket.end()
        if (lines.length === 2) serve(lines[1], socket, reader)
      }
    })
  })
  return new Promise((resolve) =>
    server.listen(path, () =>
      resolve({
        path,
        close: () => {
          for (const socket of sockets) socket.destroy()
          server.close()
          rmSync(dir, { recursive: true })
        },
      }),
    ),
  )
}

test('a call says hello, then answers or refuses with a code', async () => {
  const daemon = await fakeDaemon((request, socket) => {
    if (request.method === 'container.ls') socket.write(JSON.stringify({ result: [{ id: 'a', all: request.arguments.all }] }) + '\n')
    else socket.write(JSON.stringify({ error: 'no such container: x', code: 'not_found' }) + '\n')
  })
  try {
    assert.deepEqual(await call('container.ls', { all: true }, { path: daemon.path }), [{ id: 'a', all: true }])
    await assert.rejects(call('container.inspect', { name: 'x' }, { path: daemon.path }), (e) => e.code === 'not_found')
  } finally {
    daemon.close()
  }
})

test('a stream carries output, events, control and its end', async () => {
  const daemon = await fakeDaemon((request, socket, reader) => {
    socket.write(JSON.stringify({ result: { stream: true } }) + '\n')
    socket.write(encode(Channel.event, JSON.stringify({ progress: 'starting' })))
    const pump = () => {
      let next
      while ((next = reader.frame())) {
        if (next.channel === Channel.stdin && next.payload.length) socket.write(encode(Channel.stdout, next.payload))
        if (next.channel === Channel.control) socket.write(encode(Channel.stderr, next.payload))
        if (next.channel === Channel.stdin && !next.payload.length)
          socket.write(encode(Channel.end, JSON.stringify({ result: { exitCode: 0, name: request.arguments.name } })))
      }
    }
    pump()
    socket.on('data', () => setImmediate(pump))
  })
  try {
    const s = stream('terminal.open', { name: 'web' }, { path: daemon.path })
    const seen = { stdout: '', stderr: '', events: [] }
    s.on('stdout', (d) => (seen.stdout += d))
    s.on('stderr', (d) => (seen.stderr += d))
    s.on('event', (e) => seen.events.push(e))
    s.stdin(Buffer.from('ls\n'))
    s.control({ resize: { rows: 40, cols: 120 } })
    s.closeStdin()
    const result = await new Promise((resolve, reject) => (s.on('end', resolve), s.on('error', reject)))
    assert.deepEqual(result, { exitCode: 0, name: 'web' })
    assert.equal(seen.stdout, 'ls\n')
    assert.deepEqual(JSON.parse(seen.stderr), { resize: { rows: 40, cols: 120 } })
    assert.deepEqual(seen.events, [{ progress: 'starting' }])
  } finally {
    daemon.close()
  }
})

test('a daemon of another API, or older than API 1, says to restart it', async () => {
  for (const [hello, says] of [
    [{ error: 'this daemon serves API 2; send hello with api 2', code: 'version' }, 'this daemon serves API 2'],
    [{ error: 'unsupported operation: hello' }, 'does not speak API 1'],
  ]) {
    const daemon = await fakeDaemon(() => {}, hello)
    try {
      await assert.rejects(call('container.ls', {}, { path: daemon.path }), (e) => e.code === 'version' && e.message.includes(says) && e.message.endsWith(RESTART))
      const s = stream('terminal.open', {}, { path: daemon.path })
      const error = await new Promise((resolve) => s.on('error', resolve))
      assert.equal(error.code, 'version')
      assert.ok(error.message.endsWith(RESTART), error.message)
    } finally {
      daemon.close()
    }
  }
})

test('an unknown method names the daemon version', async () => {
  const daemon = await fakeDaemon((request, socket) => socket.write(JSON.stringify({ error: `unknown method ${request.method}`, code: 'unsupported' }) + '\n'))
  try {
    await assert.rejects(
      call('machine.endpoints', { name: 'a' }, { path: daemon.path }),
      (e) => e.code === 'unsupported' && e.message.startsWith('unknown method machine.endpoints: the daemon is version 0.0.9') && e.message.endsWith(RESTART),
    )
  } finally {
    daemon.close()
  }
})
