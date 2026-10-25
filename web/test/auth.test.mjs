import assert from 'node:assert/strict'
import { test } from 'node:test'
import { COOKIE, admit, cookieValue } from '../auth.mjs'

const opts = { port: 3417, token: 'a'.repeat(48) }

test('rejects foreign Host headers (DNS rebinding)', () => {
  for (const host of ['evil.example:3417', '127.0.0.1:80', '', undefined, '192.168.1.5:3417']) {
    assert.equal(admit({ host, url: '/', cookie: `${COOKIE}=${opts.token}` }, opts).kind, 'reject', String(host))
  }
})

test('token exchange sets a strict HttpOnly cookie and strips the token', () => {
  const d = admit({ host: '127.0.0.1:3417', url: `/containers?x=1&token=${opts.token}` }, opts)
  assert.equal(d.kind, 'redirect')
  assert.equal(d.location, '/containers?x=1')
  assert.match(d.cookie, /HttpOnly; SameSite=Strict; Path=\//)
})

test('wrong or missing credentials are refused', () => {
  assert.equal(admit({ host: 'localhost:3417', url: '/?token=nope' }, opts).kind, 'reject')
  assert.equal(admit({ host: 'localhost:3417', url: '/' }, opts).kind, 'reject')
  assert.equal(admit({ host: 'localhost:3417', url: '/', cookie: `${COOKIE}=${'b'.repeat(48)}` }, opts).kind, 'reject')
})

test('valid cookie is admitted', () => {
  assert.equal(admit({ host: 'localhost:3417', url: '/images', cookie: `other=1; ${COOKIE}=${opts.token}` }, opts).kind, 'allow')
  assert.equal(cookieValue('a=1; b=2=3', 'b'), '2=3')
})
