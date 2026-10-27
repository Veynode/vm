import assert from 'node:assert/strict'
import { test } from 'node:test'
import { admit } from '../auth.mjs'

const opts = { port: 3417 }

test('rejects foreign Host headers (DNS rebinding)', () => {
  for (const host of ['evil.example:3417', '127.0.0.1:80', '', undefined, '192.168.1.5:3417']) {
    assert.equal(admit({ host }, opts).kind, 'reject', String(host))
  }
})

test('admits the loopback origin with no sign-in', () => {
  for (const host of ['127.0.0.1:3417', 'localhost:3417', 'LOCALHOST:3417']) {
    assert.equal(admit({ host }, opts).kind, 'allow', host)
  }
})
