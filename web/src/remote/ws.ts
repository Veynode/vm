// WebSocket link: `GET wss://<gateway>/v1/desktop/ws` (plan §3.1). One binary
// message per unit (handshake unit, gateway notice or secure record), video
// included (one access unit per message, §3.4). No datagrams: pointer moves go on the reliable path, coalesced by
// the caller. Backpressure is estimated from application pings (§4), not
// from `bufferedAmount`.

import type { Link, LinkHandlers, Opener } from './transport'

export const WS_PATH = '/v1/desktop/ws'

export interface WebSocketOptions {
  /** Injected for tests; defaults to the browser's WebSocket. */
  WebSocket?: typeof WebSocket
  /** A gateway serving several hosts routes `/v1/desktop/ws/<host>`. */
  host?: string
}

export function webSocketOpener(options: WebSocketOptions = {}): Opener {
  return (origin: string, handlers: LinkHandlers, signal: AbortSignal): Promise<Link> =>
    new Promise((resolve, reject) => {
      const Ctor = options.WebSocket ?? globalThis.WebSocket
      if (!Ctor) return reject(new Error('WebSocket unsupported'))
      const url = new URL(options.host ? `${WS_PATH}/${encodeURIComponent(options.host)}` : WS_PATH, origin)
      url.protocol = url.protocol === 'http:' ? 'ws:' : 'wss:'
      const socket = new Ctor(url.href)
      socket.binaryType = 'arraybuffer'
      let open = false
      const onAbort = () => socket.close(1000, 'aborted')
      signal.addEventListener('abort', onAbort, { once: true })
      socket.onopen = () => {
        open = true
        signal.removeEventListener('abort', onAbort)
        resolve(link)
      }
      socket.onmessage = (m) => {
        if (typeof m.data === 'string') return
        const record = new Uint8Array(m.data as ArrayBuffer)
        if (record.length) handlers.onUnit(record, false)
      }
      socket.onclose = (e) => {
        signal.removeEventListener('abort', onAbort)
        if (!open) reject(new Error(`WebSocket closed before opening (${e.code})`))
        else handlers.onClose(e.code, e.reason)
      }
      const link: Link = {
        kind: 'ws',
        datagrams: false,
        send(record) {
          if (socket.readyState === 1) socket.send(record as Uint8Array<ArrayBuffer>)
        },
        sendDatagram() {
          return false
        },
        close(code = 1000, reason = '') {
          socket.close(code, reason)
        },
      }
    })
}
