// Datagram video reassembly (plan §3.4, WebTransport `datagram` mode). Each
// fragment is `u32 epoch, u32 frame_seq, u16 idx, u16 count, bytes` inside its
// own secure record (kind 8); the fragments of one frame concatenate to its
// video_v2 payload, header included (crates/remote-proto/src/frag.rs). At most three frames are buffered. A frame still missing
// fragments at its deadline is dropped, and so is every delta after it until a
// key frame arrives, while a keyframe request goes out. No FEC.
//
// Time is passed in (`now`, milliseconds) so the logic runs without timers;
// the caller calls `tick` from a timer or on each datagram.

import type { Fragment } from './proto'

/** Frames held at once (plan §3.4). */
export const MAX_FRAMES = 3

interface Pending {
  frameSeq: number
  count: number
  parts: (Uint8Array | undefined)[]
  have: number
  deadline: number
}

export interface ReassemblyResult {
  /** Complete video_v2 payloads, in frame order, safe to decode. */
  frames: Uint8Array[]
  /** A frame was lost: the decoder must wait for a key frame and one must be requested. */
  needKey: boolean
}

export interface ReassemblyStats {
  delivered: number
  /** Frames dropped incomplete (deadline or eviction). */
  lost: number
  /** Complete deltas discarded while waiting for a key frame. */
  skipped: number
  /** Fragments for frames already delivered or dropped. */
  late: number
}

/** Byte 8 of a video_v2 payload, in fragment 0: bit 0 is key. */
const isKey = (first: Uint8Array) => first.length > 8 && (first[8] & 1) === 1

export class Reassembler {
  private epoch = -1
  private frames = new Map<number, Pending>()
  /** Last frame delivered or dropped; anything at or below it is late. */
  private last = -1
  /** Waiting for a key frame: deltas are discarded. */
  private waitKey = true
  private readonly deadlineMs: () => number
  readonly stats: ReassemblyStats = { delivered: 0, lost: 0, skipped: 0, late: 0 }

  /** `deadlineMs` is read for each new frame: `clamp(max(2 × frame interval, 1.5 × RTT), 33, 150)` per plan §3.4. */
  constructor(deadlineMs: number | (() => number) = 100) {
    this.deadlineMs = typeof deadlineMs === 'number' ? () => deadlineMs : deadlineMs
  }

  push(fragment: Fragment, now: number): ReassemblyResult {
    const result: ReassemblyResult = { frames: [], needKey: false }
    if (fragment.epoch !== this.epoch) {
      // A new epoch (encoder restart, resize) starts from its own key frame.
      if (this.epoch !== -1 && (fragment.epoch - this.epoch) >>> 0 > 0x7fff_ffff) {
        this.stats.late++
        return result
      }
      this.epoch = fragment.epoch
      this.frames.clear()
      this.last = -1
      this.waitKey = true
    }
    if (this.last !== -1 && fragment.frameSeq <= this.last) {
      this.stats.late++
      return this.drain(now, result)
    }
    let frame = this.frames.get(fragment.frameSeq)
    if (!frame) {
      frame = { frameSeq: fragment.frameSeq, count: fragment.count, parts: new Array(fragment.count), have: 0, deadline: now + this.deadlineMs() }
      this.frames.set(fragment.frameSeq, frame)
      if (this.frames.size > MAX_FRAMES) this.dropOldest(result)
    }
    if (fragment.count === frame.count && !frame.parts[fragment.idx]) {
      frame.parts[fragment.idx] = fragment.data.slice()
      frame.have++
    }
    return this.drain(now, result)
  }

  /** Checks deadlines with no new fragment. */
  tick(now: number): ReassemblyResult {
    return this.drain(now, { frames: [], needKey: false })
  }

  /** Earliest pending deadline, for arming a timer. */
  nextDeadline(): number | undefined {
    let next: number | undefined
    for (const f of this.frames.values()) if (next === undefined || f.deadline < next) next = f.deadline
    return next
  }

  private ordered() {
    return [...this.frames.values()].sort((a, b) => a.frameSeq - b.frameSeq)
  }

  private lose(frame: Pending, result: ReassemblyResult) {
    this.frames.delete(frame.frameSeq)
    this.last = Math.max(this.last, frame.frameSeq)
    this.stats.lost++
    this.waitKey = true
    result.needKey = true
  }

  private dropOldest(result: ReassemblyResult) {
    const oldest = this.ordered()[0]
    if (oldest) this.lose(oldest, result)
  }

  private complete(frame: Pending): Uint8Array {
    let size = 0
    for (const p of frame.parts) size += p!.length
    const out = new Uint8Array(size)
    let at = 0
    for (const p of frame.parts) {
      out.set(p!, at)
      at += p!.length
    }
    return out
  }

  private drain(now: number, result: ReassemblyResult): ReassemblyResult {
    // A complete key frame anywhere supersedes everything before it.
    const ordered = this.ordered()
    const key = ordered.findLast((f) => f.have === f.count && isKey(f.parts[0]!))
    if (key) {
      for (const older of ordered) {
        if (older.frameSeq >= key.frameSeq) break
        this.frames.delete(older.frameSeq)
        if (older.have === older.count) this.stats.skipped++
        else this.stats.lost++
      }
      this.waitKey = false
      this.last = key.frameSeq - 1
    }
    for (;;) {
      const front = this.ordered()[0]
      if (!front) break
      if (front.have !== front.count) {
        if (now < front.deadline) break
        this.lose(front, result)
        continue
      }
      const gap = this.last !== -1 && front.frameSeq !== this.last + 1
      if (!isKey(front.parts[0]!)) {
        // A delta after a gap waits, in case the missing frame is only late;
        // past its deadline the missing frame counts as lost.
        if (gap && !this.waitKey) {
          if (now < front.deadline) break
          this.stats.lost++
          this.waitKey = true
        }
        if (this.waitKey) {
          this.frames.delete(front.frameSeq)
          this.last = front.frameSeq
          this.stats.skipped++
          result.needKey = true
          continue
        }
      }
      this.frames.delete(front.frameSeq)
      this.last = front.frameSeq
      this.waitKey = false
      this.stats.delivered++
      result.frames.push(this.complete(front))
    }
    return result
  }
}
