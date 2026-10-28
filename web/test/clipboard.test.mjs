import assert from 'node:assert/strict'
import { test } from 'node:test'
import { MAX_CLIPBOARD, ShortcutMapper, clipboardText } from '../src/desktop/clipboard.ts'

const k = (code, down) => ({ t: 'key', code, down })

test('on a Mac, ⌘C/X/A/Z reach the Guest as Ctrl and ⌘ alone is held back', () => {
  const m = new ShortcutMapper(true)
  assert.deepEqual(m.key('MetaLeft', true, false).keys, [], 'held until the next key decides')
  const copy = m.key('KeyC', true, false)
  assert.deepEqual(copy.keys, [k('ControlLeft', true), k('KeyC', true)])
  assert.equal(copy.copy, true)
  assert.equal(copy.allowDefault, false)
  // ⌘ released first: the shortcut's own release sends Ctrl up.
  assert.deepEqual(m.key('MetaLeft', false, false).keys, [])
  assert.deepEqual(m.key('KeyC', false, false).keys, [k('KeyC', false), k('ControlLeft', false)])
  // ⌘⇧Z: Shift goes through without deciding the ⌘ press.
  m.key('MetaLeft', true, false)
  assert.deepEqual(m.key('ShiftLeft', true, false).keys, [k('ShiftLeft', true)])
  assert.deepEqual(m.key('KeyZ', true, false).keys, [k('ControlLeft', true), k('KeyZ', true)])
})

test('on a Mac, ⌘V waits for the page paste and other ⌘ keys stay Super', () => {
  const m = new ShortcutMapper(true)
  m.key('MetaLeft', true, false)
  const paste = m.key('KeyV', true, false)
  assert.deepEqual(paste, { keys: [], allowDefault: true, paste: true, copy: false })
  assert.deepEqual(m.key('KeyV', false, false).keys, [])
  assert.deepEqual(m.pasteKeys(), [k('ControlLeft', true), k('KeyV', true), k('KeyV', false), k('ControlLeft', false)])
  m.key('MetaLeft', false, false)
  // ⌘Tab-like shortcuts are the Guest's Super shortcuts.
  m.key('MetaLeft', true, false)
  assert.deepEqual(m.key('KeyE', true, false).keys, [k('MetaLeft', true), k('KeyE', true)])
  assert.deepEqual(m.key('KeyE', false, false).keys, [k('KeyE', false)])
  assert.deepEqual(m.key('MetaLeft', false, false).keys, [k('MetaLeft', false)])
  // A ⌘ tap is still Super.
  m.key('MetaRight', true, false)
  assert.deepEqual(m.key('MetaRight', false, false).keys, [k('MetaRight', true), k('MetaRight', false)])
})

test('on a Mac, ⌘ held before the canvas had focus still maps by the event flag', () => {
  const m = new ShortcutMapper(true)
  const all = m.key('KeyA', true, false, true)
  assert.deepEqual(all.keys, [k('ControlLeft', true), k('KeyA', true)])
  assert.deepEqual(m.key('KeyA', false, false, true).keys, [k('KeyA', false), k('ControlLeft', false)])
  assert.equal(m.key('KeyC', true, false, true).copy, true)
  m.key('KeyC', false, false, true)
  assert.equal(m.key('KeyV', true, false, true).paste, true)
  m.key('KeyV', false, false, true)
  // Without the flag a plain key is typed, and other keys stay plain under the flag alone.
  assert.deepEqual(m.key('KeyA', true, false, false).keys, [k('KeyA', true)])
  assert.deepEqual(m.key('KeyE', true, false, true).keys, [k('KeyE', true)])
})

test('elsewhere Ctrl is the Guest modifier and only Ctrl+V waits for the page', () => {
  const m = new ShortcutMapper(false)
  assert.deepEqual(m.key('ControlLeft', true, true).keys, [k('ControlLeft', true)])
  assert.equal(m.key('KeyC', true, true).copy, true)
  const paste = m.key('KeyV', true, true)
  assert.equal(paste.paste, true)
  assert.deepEqual(paste.keys, [])
  assert.deepEqual(m.key('KeyV', false, true).keys, [])
  assert.deepEqual(m.pasteKeys(), [k('KeyV', true), k('KeyV', false)])
  assert.deepEqual(m.key('KeyV', true, false).keys, [k('KeyV', true)], 'plain V is typed')
})

test('a macOS Guest on a Mac keeps ⌘ and only ⌘V waits for the page', () => {
  const m = new ShortcutMapper(true, true)
  assert.deepEqual(m.key('MetaLeft', true, false, true).keys, [k('MetaLeft', true)], 'sent at once, not held back')
  const copy = m.key('KeyC', true, false, true)
  assert.deepEqual(copy.keys, [k('KeyC', true)])
  assert.equal(copy.copy, true)
  m.key('KeyC', false, false, true)
  const paste = m.key('KeyV', true, false, true)
  assert.deepEqual(paste, { keys: [], allowDefault: true, paste: true, copy: false })
  assert.deepEqual(m.key('KeyV', false, false, true).keys, [])
  assert.deepEqual(m.pasteKeys(), [k('KeyV', true), k('KeyV', false)])
  assert.deepEqual(m.key('KeyV', true, true, false).keys, [k('KeyV', true)], 'Ctrl+V is typed through')
})

test('clipboard text is bounded in UTF-8 bytes', () => {
  assert.equal(clipboardText('你好'), '你好')
  assert.equal(clipboardText('a'.repeat(MAX_CLIPBOARD)), 'a'.repeat(MAX_CLIPBOARD))
  assert.equal(clipboardText('é'.repeat(MAX_CLIPBOARD / 2 + 1)), null)
})
