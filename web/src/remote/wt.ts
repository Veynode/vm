// WebTransport link: `CONNECT https://<gateway>/v1/desktop` (plan §3.1).
//
//   bidirectional stream 0   reliable units, each `u32 len | unit` (proto.ts classifyUnit)
//   incoming uni streams     one video secure record per stream, ended by FIN;
//                            a reset (deadline passed, superseded) → need_key
//   datagrams                one secure record each: pointer, ping copies,
//                            and video fragments in `datagram` mode

import { LengthReader, lengthPrefixed } from './proto'
import type { Link, LinkHandlers, Opener } from './transport'

export const WT_PATH = '/v1/desktop'

export interface WebTransportOptions {
  /** Injected for tests; defaults to the browser's WebTransport. */
  WebTransport?: typeof WebTransport
  /** The gateway's self-signed certificate (`server_certificate_sha256` from /v1/capabilities). */
  serverCertificateSha256?: string
  /** A gateway serving several hosts routes `/v1/desktop/<host>`. */
  host?: string
}

async function readAll(stream: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const reader = stream.getReader()
  const chunks: Uint8Array[] = []
  let size = 0
  for (;;) {
    const { value, done } = await reader.read()
    if (done) break
    chunks.push(value)
    size += value.length
  }
  if (chunks.length === 1) return chunks[0]
  const out = new Uint8Array(size)
  let at = 0
  for (const c of chunks) {
    out.set(c, at)
    at += c.length
  }
  return out
}

export function webTransportOpener(options: WebTransportOptions = {}): Opener {
  return async (origin: string, handlers: LinkHandlers, signal: AbortSignal): Promise<Link> => {
    const Ctor = options.WebTransport ?? globalThis.WebTransport
    if (!Ctor) throw new Error('WebTransport unsupported')
    const url = new URL(options.host ? `${WT_PATH}/${encodeURIComponent(options.host)}` : WT_PATH, origin).href
    const hash = options.serverCertificateSha256
    const wt = new Ctor(
      url,
      hash ? { serverCertificateHashes: [{ algorithm: 'sha-256', value: Uint8Array.from(hash.match(/../g)!, (h) => parseInt(h, 16)) }] } : undefined,
    )
    const onAbort = () => wt.close({ closeCode: 0, reason: 'aborted' })
    signal.addEventListener('abort', onAbort, { once: true })
    try {
      await wt.ready
    } catch (error) {
      signal.removeEventListener('abort', onAbort)
      throw error
    }
    const bidi = await wt.createBidirectionalStream()
    signal.removeEventListener('abort', onAbort)
    const writer = bidi.writable.getWriter()
    const datagramWriter = wt.datagrams.writable.getWriter()
    let closed = false

    void (async () => {
      const reader = bidi.readable.getReader()
      const units = new LengthReader()
      try {
        for (;;) {
          const { value, done } = await reader.read()
          if (done) break
          for (const record of units.push(value)) handlers.onUnit(record, false)
        }
      } catch {
        // The session's `closed` reports why.
      }
    })()

    void (async () => {
      const streams = wt.incomingUnidirectionalStreams.getReader()
      for (;;) {
        let next: ReadableStreamReadResult<ReadableStream<Uint8Array>>
        try {
          next = await streams.read()
        } catch {
          return
        }
        if (next.done) return
        readAll(next.value).then(
          (record) => record.length && handlers.onUnit(record, false),
          () => !closed && handlers.onVideoReset(),
        )
      }
    })()

    void (async () => {
      const reader = wt.datagrams.readable.getReader()
      try {
        for (;;) {
          const { value, done } = await reader.read()
          if (done) break
          if (value.length) handlers.onUnit(value, true)
        }
      } catch {
        // As above.
      }
    })()

    wt.closed.then(
      (info) => {
        closed = true
        handlers.onClose(info.closeCode ?? 0, info.reason ?? '')
      },
      (error) => {
        closed = true
        handlers.onClose(0, String(error))
      },
    )

    return {
      kind: 'wt',
      datagrams: true,
      send(record) {
        if (!closed) writer.write(lengthPrefixed(record) as Uint8Array<ArrayBuffer>).catch(() => {})
      },
      sendDatagram(record) {
        if (closed || (datagramWriter.desiredSize ?? 0) <= 0) return false
        datagramWriter.write(record as Uint8Array<ArrayBuffer>).catch(() => {})
        return true
      },
      close(code = 0, reason = '') {
        if (closed) return
        closed = true
        wt.close({ closeCode: code, reason })
      },
    }
  }
}
