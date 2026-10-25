// Request admission for the local web UI, kept free of I/O so it is testable.
//
// - Host must be the loopback origin the server bound (DNS rebinding guard).
// - A per-launch token arrives once as ?token=… and is exchanged for an
//   HttpOnly SameSite=Strict cookie; every other request must carry it.
import { timingSafeEqual } from 'node:crypto'

export const COOKIE = 'veynodevm_ui'

export function allowedHosts(port) {
  return new Set([`127.0.0.1:${port}`, `localhost:${port}`])
}

function same(a, b) {
  const x = Buffer.from(String(a))
  const y = Buffer.from(String(b))
  return x.length === y.length && timingSafeEqual(x, y)
}

export function cookieValue(header, name) {
  for (const part of String(header ?? '').split(';')) {
    const [key, ...rest] = part.trim().split('=')
    if (key === name) return rest.join('=')
  }
  return undefined
}

/**
 * @returns {{ kind: 'reject', status: number, message: string }
 *   | { kind: 'redirect', location: string, cookie: string }
 *   | { kind: 'allow' }}
 */
export function admit({ host, url, cookie }, { port, token }) {
  if (!allowedHosts(port).has(String(host ?? '').toLowerCase())) {
    return { kind: 'reject', status: 421, message: 'Misdirected request: unexpected Host header' }
  }
  const parsed = new URL(url, `http://127.0.0.1:${port}`)
  const offered = parsed.searchParams.get('token')
  if (offered !== null) {
    if (!same(offered, token)) return { kind: 'reject', status: 401, message: 'Invalid token' }
    parsed.searchParams.delete('token')
    return {
      kind: 'redirect',
      location: parsed.pathname + (parsed.search || ''),
      cookie: `${COOKIE}=${token}; HttpOnly; SameSite=Strict; Path=/`,
    }
  }
  const presented = cookieValue(cookie, COOKIE)
  if (presented === undefined || !same(presented, token)) {
    return { kind: 'reject', status: 401, message: 'Open the URL printed by `veynodevm ui` to sign in.' }
  }
  return { kind: 'allow' }
}
