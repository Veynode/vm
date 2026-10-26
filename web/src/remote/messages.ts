// JSON shapes inside secure records, as the host sends and reads them
// (`crates/media-local/src/remote/session.rs`, `hub.rs`), plus the gateway's
// notices and capabilities (`crates/gateway/src/web.rs`).

import { CLOSE } from './proto'

/** Kind 1, host → browser. `view.fallback` means no crop track: zoom client-side. */
export type HostEvent =
  | { event: 'session'; session_id: string; vm: string; codec: 'h264' | 'h265'; expires_at: number; input: boolean }
  | { event: 'ready'; vm: string; backend: string; codec: string }
  | { event: 'viewers'; n: number }
  | { event: 'view'; fallback: 'full'; message?: string }
  | { event: 'rate'; bps: number; scale: number }
  | { event: 'lease'; mode: string; epoch: number; holder?: string }
  | { event: 'input_error'; message: string }
  | { event: 'error'; message: string }
  /** Sent just before the host ends the route; the gateway does not carry the code. */
  | { event: 'close'; code: number }
  | { event: string; [field: string]: unknown }

export function parseHostEvent(value: unknown): HostEvent | undefined {
  if (!value || typeof value !== 'object' || typeof (value as { event?: unknown }).event !== 'string') return undefined
  return value as HostEvent
}

/** What a close code (host `close` event, `error` handshake message, socket close) means to a person. */
export function closeReason(code: number): string {
  switch (code) {
    case CLOSE.unauthorized:
      return 'Not authorized, or the session expired'
    case CLOSE.replaced:
      return 'Replaced by another connection'
    case CLOSE.revoked:
      return 'Access was revoked'
    case CLOSE.rateLimited:
      return 'Too many requests; try again shortly'
    case CLOSE.invalid:
      return 'Too many invalid records'
    case CLOSE.retry:
      return 'Host unavailable; retrying'
    default:
      return `Closed (${code})`
  }
}

/** Kind 4, browser → host. */
export const keyframeRequest = () => ({ keyframe_req: true })

/** Kind 3: `seq` strictly increasing per session. Events as `web/src/desktop/client.ts` InputEvent. */
export const inputMessage = (seq: number, events: unknown[]) => ({ seq, events })

/** Kind 6, once a second: rtt in ms, loss as a fraction 0..1. */
export const statsMessage = (s: { rtt: number; loss: number; fps: number }) => ({ rtt: s.rtt, loss: Math.min(1, Math.max(0, s.loss)), fps: s.fps })

/** Kind 5 ping; the host echoes `t_send_us` and adds `t_peer_us` (host unix µs). */
export const pingMessage = (id: number, tSendUs: number) => ({ id, t_send_us: tSendUs })

export interface Pong {
  id: number
  t_send_us: number
  t_peer_us: number
}

/**
 * Round trip and host clock offset (NTP style, display only) from a pong
 * received at `tRecvUs` on the same clock as `t_send_us`.
 */
export function pongTiming(pong: Pong, tRecvUs: number): { rttMs: number; offsetMs: number } {
  const rtt = tRecvUs - pong.t_send_us
  return { rttMs: rtt / 1000, offsetMs: (pong.t_peer_us - (pong.t_send_us + rtt / 2)) / 1000 }
}

/** `0xff | JSON` from the gateway: unauthenticated, display only. */
export interface GatewayNotice {
  gateway?: { p50_us: number; p95_us: number; samples: number }
}

/** `GET /v1/capabilities`. */
export interface Capabilities {
  protocol: number
  transports: ('wt' | 'wss' | 'webrtc')[]
  video: 'stream' | 'datagram'
  hosts_online: number
  /** Self-signed mode: pass to WebTransport `serverCertificateHashes`. */
  server_certificate_sha256?: string
}

export async function fetchCapabilities(origin: string, fetcher: typeof fetch = fetch): Promise<Capabilities> {
  const response = await fetcher(new URL('/v1/capabilities', origin).href, { credentials: 'omit', cache: 'no-store' })
  if (!response.ok) throw new Error(`capabilities: HTTP ${response.status}`)
  const caps = (await response.json()) as Capabilities
  if (!Array.isArray(caps?.transports)) throw new Error('capabilities: malformed')
  return caps
}

/** The transport kinds (transport.ts naming) a gateway offers, in its own words. */
export function offeredKinds(caps: Capabilities): ('wt' | 'ws' | 'webrtc')[] {
  return caps.transports.map((t) => (t === 'wss' ? 'ws' : t))
}
