import assert from 'node:assert/strict'
import { test } from 'node:test'
import { upgradeTerminal } from '../terminal.mjs'

const origins = new Set(['http://127.0.0.1:3417'])

function attempt(url, headers = {}) {
  let written = ''
  const socket = {
    end: (text) => (written += text),
    write: (text) => (written += text),
  }
  const req = {
    url,
    headers: {
      origin: 'http://127.0.0.1:3417',
      upgrade: 'websocket',
      'sec-websocket-key': 'dGhlIHNhbXBsZSBub25jZQ==',
      'sec-websocket-version': '13',
      ...headers,
    },
  }
  // A rejected request never reaches spawn, so the CLI path is irrelevant.
  upgradeTerminal(req, socket, { origins })
  return written.split('\r\n')[0]
}

test('terminal upgrades are refused for other paths, origins and names', () => {
  assert.equal(attempt('/desktop/ws?id=web'), 'HTTP/1.1 404 Not Found')
  assert.equal(attempt('/terminal/ws?id=web', { origin: 'http://evil.test' }), 'HTTP/1.1 403 Forbidden')
  assert.equal(attempt('/terminal/ws?id=../x'), 'HTTP/1.1 400 Bad Request')
  assert.equal(attempt('/terminal/ws?id=web', { 'sec-websocket-version': '8' }), 'HTTP/1.1 400 Bad Request')
})
