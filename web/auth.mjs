// Request admission for the local web UI, kept free of I/O so it is testable.
//
// There is no sign-in: the server binds only 127.0.0.1 and admits a request
// when its Host is that loopback origin (DNS rebinding guard). The terminal
// and desktop WebSockets also require the exact Origin (desktop.mjs,
// terminal.mjs). Other pages a browser has open can still send plain HTTP
// requests to the UI; that is an accepted local trust boundary.

export function allowedHosts(port) {
  return new Set([`127.0.0.1:${port}`, `localhost:${port}`])
}

/**
 * @returns {{ kind: 'reject', status: number, message: string } | { kind: 'allow' }}
 */
export function admit({ host }, { port }) {
  if (!allowedHosts(port).has(String(host ?? '').toLowerCase())) {
    return { kind: 'reject', status: 421, message: 'Misdirected request: unexpected Host header' }
  }
  return { kind: 'allow' }
}
