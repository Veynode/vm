// Crop view for zooming on a phone. The host can encode a part of the
// screen (`View` in every video_v2 header, source pixels). While the user
// pinches or pans, the frame already on screen is CSS-scaled at once; 150 ms
// after the gesture settles, a kind 4 control `{"view":{x,y,w,h,out_w,out_h}}`
// asks the host for that area at the viewer's physical pixel size, and the
// host answers with a key frame whose View matches. `{"view":null}` returns
// to the full screen.
//
// The invariant: the element always shows `target`. Input is mapped through
// `target`, never through the frame on screen, so a click lands where it
// looks like it does even before the cropped frame arrives. The page keeps
// the element's aspect ratio equal to `target`'s.

import type { View } from './proto'

export const VIEW_DEBOUNCE_MS = 150
/** Deepest zoom: an eighth of the source either way. */
export const MAX_ZOOM = 8

export interface Rect {
  x: number
  y: number
  w: number
  h: number
}

export const fullView = (srcW: number, srcH: number): View => ({ x: 0, y: 0, w: srcW, h: srcH, srcW, srcH })
export const isFull = (v: View) => v.x === 0 && v.y === 0 && v.w === v.srcW && v.h === v.srcH

/** A point in the element (CSS px from its top-left, element `width × height`) in source pixels, clamped to the source. */
export function toSource(target: View, px: number, py: number, width: number, height: number): { x: number; y: number } {
  const x = target.x + (px / width) * target.w
  const y = target.y + (py / height) * target.h
  return {
    x: Math.min(target.srcW - 1, Math.max(0, Math.floor(x))),
    y: Math.min(target.srcH - 1, Math.max(0, Math.floor(y))),
  }
}

/**
 * Where to place the canvas holding a frame that shows `frame` so that the
 * element (`width × height`) shows `target`: CSS box in element pixels.
 */
export function placement(frame: View, target: View, width: number, height: number): Rect {
  const sx = width / target.w
  const sy = height / target.h
  return { x: (frame.x - target.x) * sx, y: (frame.y - target.y) * sy, w: frame.w * sx, h: frame.h * sy }
}

/** The kind 4 control payload for `target` shown at `outW × outH` physical pixels. */
export function viewControl(target: View, outW: number, outH: number) {
  if (isFull(target)) return { view: null }
  return { view: { x: target.x, y: target.y, w: target.w, h: target.h, out_w: Math.round(outW), out_h: Math.round(outH) } }
}

/** Keeps `w × h` inside the source with integer edges, at least the deepest zoom's size. */
function clampView(srcW: number, srcH: number, x: number, y: number, w: number, h: number): View {
  w = Math.round(Math.min(srcW, Math.max(Math.max(2, srcW / MAX_ZOOM), w)))
  h = Math.round(Math.min(srcH, Math.max(Math.max(2, srcH / MAX_ZOOM), h)))
  x = Math.round(Math.min(srcW - w, Math.max(0, x)))
  y = Math.round(Math.min(srcH - h, Math.max(0, y)))
  return { x, y, w, h, srcW, srcH }
}

export interface PanZoomOptions {
  /** Sends a kind 4 control payload. */
  send(control: ReturnType<typeof viewControl>): void
  debounceMs?: number
  setTimer?: (run: () => void, ms: number) => unknown
  clearTimer?: (handle: unknown) => void
}

export class PanZoom {
  /** What the element shows (and input maps through). */
  target: View
  /** What the frame on screen shows, from its video_v2 header. */
  frame: View
  private out = { w: 0, h: 0 }
  private timer: unknown
  private sent: string
  private readonly options: Required<PanZoomOptions>

  constructor(srcW: number, srcH: number, options: PanZoomOptions) {
    this.target = this.frame = fullView(srcW, srcH)
    this.sent = JSON.stringify(viewControl(this.target, 0, 0))
    this.options = {
      debounceMs: VIEW_DEBOUNCE_MS,
      setTimer: (run, ms) => setTimeout(run, ms),
      clearTimer: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
      ...options,
    }
  }

  /** The element's CSS size and devicePixelRatio, for out_w/out_h. */
  setOutput(cssW: number, cssH: number, dpr: number) {
    this.out = { w: cssW * dpr, h: cssH * dpr }
  }

  /** Zooms by `factor` (> 1 in) keeping the source point under (fx, fy), fractions of the element, fixed. */
  zoomAt(factor: number, fx: number, fy: number) {
    const t = this.target
    const w = t.w / factor
    const h = t.h / factor
    const ax = t.x + fx * t.w
    const ay = t.y + fy * t.h
    this.change(clampView(t.srcW, t.srcH, ax - fx * w, ay - fy * h, w, h))
  }

  /** Pans by a fraction of the element (a finger moving right by 0.1 shows what was to the left). */
  panBy(fx: number, fy: number) {
    const t = this.target
    this.change(clampView(t.srcW, t.srcH, t.x - fx * t.w, t.y - fy * t.h, t.w, t.h))
  }

  reset() {
    this.change(fullView(this.target.srcW, this.target.srcH))
  }

  /** A frame was painted: its View. A new source size (a resize) drops the zoom. */
  painted(view: View) {
    if (view.srcW !== this.target.srcW || view.srcH !== this.target.srcH) {
      this.target = fullView(view.srcW, view.srcH)
      // Still cropped to the old size's area: ask for the full screen.
      this.sent = isFull(view) ? JSON.stringify(viewControl(this.target, 0, 0)) : ''
      if (!isFull(view)) this.schedule()
    }
    this.frame = view
  }

  /** Pointer position for input (EventWire move / kind 9): source pixels and the source size. */
  pointer(px: number, py: number, width: number, height: number) {
    return { ...toSource(this.target, px, py, width, height), w: this.target.srcW, h: this.target.srcH }
  }

  /** CSS box for the canvas now. */
  placement(width: number, height: number): Rect {
    return placement(this.frame, this.target, width, height)
  }

  close() {
    if (this.timer !== undefined) this.options.clearTimer(this.timer)
    this.timer = undefined
  }

  private change(view: View) {
    this.target = view
    this.schedule()
  }

  private schedule() {
    this.close()
    this.timer = this.options.setTimer(() => {
      this.timer = undefined
      const control = viewControl(this.target, this.out.w, this.out.h)
      const json = JSON.stringify(control)
      if (json === this.sent) return
      this.sent = json
      this.options.send(control)
    }, this.options.debounceMs)
  }
}
