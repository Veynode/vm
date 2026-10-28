// Guest clipboard for the browser desktop (Linux desktops and macOS containers;
// docs/browser-desktop.md).
// Text only, at most 200 KiB.
//
// Automatic (the default, kept per desktop): every Guest copy is written to the
// page's clipboard, and the page's text goes to the Guest when the person
// pastes. On a Mac, ⌘A/C/V/X/Z reach the Guest as Ctrl shortcuts; any other ⌘
// shortcut still reaches it as Super. A macOS Guest takes ⌘ as it is.
// Manual: no mapping and no sync; the toolbar sends the page's clipboard to the
// Guest or copies the Guest's.
//
// Relay messages: `{"clipboard":"TEXT"}`, `{"clipboard_watch":bool}`,
// `{"clipboard_get":true}`; the Guest answers `{"event":"clipboard","text"|"error"}`.

export type ClipboardMode = 'auto' | 'manual'

export const MAX_CLIPBOARD = 200 * 1024

export type KeyEvent = { t: 'key'; code: string; down: boolean }

/** What one key press or release does in automatic mode. */
export interface KeyAction {
  /** Key events for the Guest, in order. */
  keys: KeyEvent[]
  /** Leave the browser's default action alone: a paste shortcut, so the page gets its `paste` event. */
  allowDefault: boolean
  /** A paste shortcut: send the page's text, then `pasteKeys()`. */
  paste: boolean
  /** A copy or cut shortcut: the Guest's next clipboard event is the copied text. */
  copy: boolean
}

const MAPPED = new Set(['KeyA', 'KeyC', 'KeyV', 'KeyX', 'KeyZ'])
const META = new Set(['MetaLeft', 'MetaRight'])
/** Modifiers pressed with ⌘ (⌘⇧Z) that do not decide what the ⌘ press is. */
const MODIFIERS = new Set(['ShiftLeft', 'ShiftRight', 'AltLeft', 'AltRight', 'ControlLeft', 'ControlRight'])
const key = (code: string, down: boolean): KeyEvent => ({ t: 'key', code, down })

/**
 * Turns the page's shortcuts into the Guest's. On a Mac a ⌘ press is held back
 * until the next key shows whether it is a mapped shortcut (sent as Ctrl) or
 * anything else (⌘ goes to the Guest first, as before). Elsewhere Ctrl is the
 * Guest's own modifier, and only Ctrl+V is held for the page's text; so is ⌘
 * for a macOS Guest on a Mac (`guestMac`), and only ⌘V is held.
 */
export class ShortcutMapper {
  constructor(mac: boolean, guestMac = false) {
    this.mac = mac
    this.guestMac = guestMac
    this.meta = null
    this.metaSent = false
    this.metaUsed = false
    this.mapped = new Set()
  }
  declare mac: boolean
  declare guestMac: boolean
  /** The ⌘ key held and not yet sent. */
  declare meta: string | null
  declare metaSent: boolean
  /** A mapped shortcut used this ⌘ press: its release sends nothing. */
  declare metaUsed: boolean
  /** Keys pressed as a mapped shortcut, whose release is the shortcut's. */
  declare mapped: Set<string>

  /** `meta` is the event's own ⌘ flag, which is set even when the page missed ⌘'s keydown. */
  key(code: string, down: boolean, ctrl: boolean, meta = false): KeyAction {
    const action: KeyAction = { keys: [], allowDefault: false, paste: false, copy: false }
    if (!this.mac || this.guestMac) {
      // The page's shortcut modifier is the Guest's own.
      const held = this.mac ? meta : ctrl
      if (held && code === 'KeyV' && down) {
        this.mapped.add(code)
        return { ...action, allowDefault: true, paste: true }
      }
      if (!down && this.mapped.delete(code)) return action
      return { ...action, keys: [key(code, down)], copy: down && held && (code === 'KeyC' || code === 'KeyX') }
    }
    if (META.has(code)) {
      if (down) {
        this.meta = code
        this.metaSent = this.metaUsed = false
        return action
      }
      const held = this.meta
      this.meta = null
      if (this.metaSent) return { ...action, keys: [key(code, false)] }
      // A ⌘ tap on its own is still the Guest's Super key.
      if (held && !this.metaUsed) return { ...action, keys: [key(held, true), key(held, false)] }
      return action
    }
    // ⌘ held before the canvas had focus sent no keydown here; the event's flag still shows it.
    if (down && (this.meta ? !this.metaSent : meta) && MAPPED.has(code)) {
      this.metaUsed = true
      this.mapped.add(code)
      if (code === 'KeyV') return { ...action, allowDefault: true, paste: true }
      return { ...action, keys: [key('ControlLeft', true), key(code, true)], copy: code === 'KeyC' || code === 'KeyX' }
    }
    if (!down && this.mapped.delete(code)) {
      return { ...action, keys: code === 'KeyV' ? [] : [key(code, false), key('ControlLeft', false)] }
    }
    if (down && this.meta && !this.metaSent && !MODIFIERS.has(code)) {
      this.metaSent = true
      return { ...action, keys: [key(this.meta, true), key(code, true)] }
    }
    return { ...action, keys: [key(code, down)] }
  }

  /** The Guest's paste shortcut, once its clipboard has the page's text. */
  pasteKeys(): KeyEvent[] {
    const v = [key('KeyV', true), key('KeyV', false)]
    return this.mac && !this.guestMac ? [key('ControlLeft', true), ...v, key('ControlLeft', false)] : v
  }

  /** Forget held keys (the canvas lost focus; its releases are sent separately). */
  reset() {
    this.meta = null
    this.metaSent = this.metaUsed = false
    this.mapped.clear()
  }
}

/** Text the page may send: at most MAX_CLIPBOARD bytes of UTF-8. */
export function clipboardText(text: string): string | null {
  return new TextEncoder().encode(text).length <= MAX_CLIPBOARD ? text : null
}

export function isMac(): boolean {
  return typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent)
}

const storageKey = (name: string) => `veynodevm.clipboard.${name}`

export function savedMode(name: string): ClipboardMode {
  try {
    return localStorage.getItem(storageKey(name)) === 'manual' ? 'manual' : 'auto'
  } catch {
    return 'auto'
  }
}

export function saveMode(name: string, mode: ClipboardMode) {
  try {
    localStorage.setItem(storageKey(name), mode)
  } catch {}
}
