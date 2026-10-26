// Picks the outer transport to the gateway (plan §2/§3.1): WebTransport, then
// WebSocket, then WebRTC, each given 3 s to open. The kind that last worked
// for a gateway origin is tried first next time (remembered in localStorage,
// which may be missing or throw; then the default order is used).
//
// Every transport carries the same secure records (secure.ts), one sequence
// space per direction whichever path a record takes.

export type TransportKind = 'wt' | 'ws' | 'webrtc'
export const DEFAULT_ORDER: readonly TransportKind[] = ['wt', 'ws', 'webrtc']
export const OPEN_TIMEOUT_MS = 3000

export interface Link {
  readonly kind: TransportKind
  /** Whether `sendDatagram` can deliver anything (WebTransport only). */
  readonly datagrams: boolean
  /** Reliable, ordered: one unit (a handshake unit or a secure record). */
  send(unit: Uint8Array): void
  /** Unreliable; false when there is no datagram path or it is full. */
  sendDatagram(record: Uint8Array): boolean
  close(code?: number, reason?: string): void
}

export interface LinkHandlers {
  /**
   * One unit: on the reliable path a handshake unit, gateway notice or secure
   * record (`classifyUnit`); from a WT uni stream or datagram (`viaDatagram`)
   * always a secure record.
   */
  onUnit(unit: Uint8Array, viaDatagram: boolean): void
  /** A video stream was reset before its end (WebTransport `stream` mode): mark need_key. */
  onVideoReset(): void
  onClose(code: number, reason: string): void
}

/** Opens one kind of transport to `origin`. Must clean up when `signal` aborts. */
export type Opener = (origin: string, handlers: LinkHandlers, signal: AbortSignal) => Promise<Link>

export interface KindStore {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
}

const storeKey = (origin: string) => `veynode.remote.transport:${origin}`

function defaultStore(): KindStore | undefined {
  try {
    return globalThis.localStorage ?? undefined
  } catch {
    return undefined
  }
}

export function rememberedKind(origin: string, store = defaultStore()): TransportKind | undefined {
  try {
    const value = store?.getItem(storeKey(origin))
    return DEFAULT_ORDER.includes(value as TransportKind) ? (value as TransportKind) : undefined
  } catch {
    return undefined
  }
}

export function remember(origin: string, kind: TransportKind, store = defaultStore()) {
  try {
    store?.setItem(storeKey(origin), kind)
  } catch {
    // Private windows and blocked storage: just use the default order next time.
  }
}

/** The order to try: the remembered kind first, then the rest of the default. */
export function attemptOrder(origin: string, store = defaultStore()): TransportKind[] {
  const first = rememberedKind(origin, store)
  return first ? [first, ...DEFAULT_ORDER.filter((k) => k !== first)] : [...DEFAULT_ORDER]
}

export interface Attempt {
  kind: TransportKind
  ok: boolean
  ms: number
  error?: string
}

export interface ConnectOptions {
  /** The gateway's origin, e.g. `https://desk.example.com`. */
  origin: string
  openers: Partial<Record<TransportKind, Opener>>
  handlers: LinkHandlers
  timeoutMs?: number
  store?: KindStore
  signal?: AbortSignal
  now?: () => number
}

export class ConnectError extends Error {
  readonly attempts: Attempt[]
  constructor(attempts: Attempt[]) {
    super(`no transport opened: ${attempts.map((a) => `${a.kind} ${a.error}`).join('; ')}`)
    this.attempts = attempts
  }
}

/** Tries each transport in order until one opens; remembers the winner. */
export async function connect(options: ConnectOptions): Promise<{ link: Link; attempts: Attempt[] }> {
  const timeoutMs = options.timeoutMs ?? OPEN_TIMEOUT_MS
  const now = options.now ?? (() => performance.now())
  const store = 'store' in options ? options.store : defaultStore()
  const attempts: Attempt[] = []
  for (const kind of attemptOrder(options.origin, store)) {
    if (options.signal?.aborted) break
    const open = options.openers[kind]
    if (!open) {
      attempts.push({ kind, ok: false, ms: 0, error: 'unavailable' })
      continue
    }
    const started = now()
    const abort = new AbortController()
    const onAbort = () => abort.abort()
    options.signal?.addEventListener('abort', onAbort)
    let timer: ReturnType<typeof setTimeout> | undefined
    try {
      const opening = open(options.origin, options.handlers, abort.signal)
      const timeout = new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`timed out after ${timeoutMs} ms`)), timeoutMs)
      })
      // A transport that opens after its timeout is closed, not leaked.
      opening.then(
        (link) => abort.signal.aborted && link.close(1000, 'superseded'),
        () => {},
      )
      const link = await Promise.race([opening, timeout])
      attempts.push({ kind, ok: true, ms: now() - started })
      remember(options.origin, kind, store)
      return { link, attempts }
    } catch (error) {
      abort.abort()
      attempts.push({ kind, ok: false, ms: now() - started, error: error instanceof Error ? error.message : String(error) })
    } finally {
      clearTimeout(timer)
      options.signal?.removeEventListener('abort', onAbort)
    }
  }
  throw new ConnectError(attempts)
}

/**
 * Keeps only the newest value and sends at most one per scheduled tick, never
 * two at once (sealing is async): pointer moves over WebTransport datagrams,
 * where an old position is worthless once a newer one exists (plan §3.2 kind 9).
 */
export class LatestWins<T> {
  private pending: { value: T } | undefined
  private scheduled = false
  private sending = false
  private readonly send: (value: T) => void | Promise<unknown>
  private readonly schedule: (run: () => void) => void
  /** Values replaced before they were sent. */
  replaced = 0

  constructor(send: (value: T) => void | Promise<unknown>, schedule: (run: () => void) => void = (run) => requestAnimationFrame(run)) {
    this.send = send
    this.schedule = schedule
  }

  offer(value: T) {
    if (this.pending) this.replaced++
    this.pending = { value }
    this.arm()
  }

  /** Drops what is pending (before a button or key, which carries its own position). */
  take(): T | undefined {
    const value = this.pending?.value
    this.pending = undefined
    return value
  }

  private arm() {
    if (this.scheduled || this.sending || !this.pending) return
    this.scheduled = true
    this.schedule(() => void this.flush())
  }

  private async flush() {
    this.scheduled = false
    const next = this.pending
    if (!next) return
    this.pending = undefined
    this.sending = true
    try {
      await this.send(next.value)
    } catch {
      // A lost pointer move is replaced by the next one.
    } finally {
      this.sending = false
      this.arm()
    }
  }
}
