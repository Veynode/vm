import { useLayoutEffect, useRef, useState } from 'react'
import type { SettingsListFrameProps } from '@ui/settings/settings-page'

/*
 * The places' sidebar: core's `ResizableSidebar` (design-system/shell.md,
 * "Widths"), left side only and kept per browser instead of in console's
 * preferences: 300px until dragged, 220–420px, never leaving the page less
 * than 300px. Below 600px the list and the page take turns, so it does not
 * drag there. The width is `--sidebar-width` on the root, which
 * `SIDEBAR_SCRIPT` sets in the head before the first paint, so a page drawn
 * on the server does not jump once it hydrates.
 */

export const SIDEBAR_WIDTH = 300
const MIN = 220
const MAX = 420
const WIDTH_KEY = 'veynodevm-sidebar-width'

function read() {
  try {
    return Number(localStorage.getItem(WIDTH_KEY)) || null
  } catch {
    return null
  }
}
function write(value: number) {
  try {
    localStorage.setItem(WIDTH_KEY, String(value))
  } catch {
    // Private windows and blocked storage keep it for this page only.
  }
}

/** The width it takes: the wanted one within its limits and what the window leaves the page. */
function fit(value: number) {
  const available = Math.max(MIN, window.innerWidth - 300)
  return Math.round(Math.min(MAX, available, Math.max(MIN, value)))
}

function setWidthVar(value: number) {
  document.documentElement.style.setProperty('--sidebar-width', value + 'px')
}

/** For the head: the saved width, fitted as `fit` does, on the root before anything is drawn. */
export const SIDEBAR_SCRIPT = `(() => {
  try {
    const saved = Number(localStorage.getItem(${JSON.stringify(WIDTH_KEY)}))
    if (!saved) return
    const available = Math.max(${MIN}, innerWidth - 300)
    const width = Math.round(Math.min(${MAX}, available, Math.max(${MIN}, saved)))
    document.documentElement.style.setProperty('--sidebar-width', width + 'px')
  } catch {}
})()`

/** `SettingsPage`'s `listFrame`: the list's column and the handle that resizes it. */
export function SidebarFrame({ className, label, view, children }: SettingsListFrameProps) {
  const ref = useRef<HTMLElement>(null)
  const drag = useRef<{ x: number; width: number } | null>(null)
  const [width, setWidth] = useState(SIDEBAR_WIDTH)
  const [saved, setSaved] = useState(SIDEBAR_WIDTH)
  const [dragging, setDragging] = useState(false)
  const apply = (value: number) => {
    const next = fit(value)
    setWidth(next)
    setWidthVar(next)
    return next
  }
  const save = (value: number) => {
    setSaved(value)
    write(value)
  }
  useLayoutEffect(() => {
    const stored = read()
    if (stored) setSaved(stored)
  }, [])
  useLayoutEffect(() => {
    apply(saved)
    // Fit the chosen width again, not the last fitted one, so a window that
    // shrinks and grows back returns the column to the width chosen.
    const resize = () => apply(saved)
    window.addEventListener('resize', resize)
    return () => window.removeEventListener('resize', resize)
  }, [saved])
  // While dragging, the whole page shows the resize cursor and nothing selects.
  useLayoutEffect(() => {
    if (!dragging) return
    const root = document.documentElement.style
    root.cursor = 'col-resize'
    root.userSelect = 'none'
    return () => {
      root.cursor = ''
      root.userSelect = ''
    }
  }, [dragging])
  return (
    <nav ref={ref} aria-label={label} data-view={view} className={className + ' min-narrow:w-[var(--sidebar-width,300px)]!'}>
      {children}
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
        aria-valuenow={width}
        aria-valuemin={MIN}
        aria-valuemax={MAX}
        tabIndex={0}
        data-dragging={dragging || undefined}
        className={
          // Panes are separated by space, not a border: the handle is that
          // space, with a grip in its middle on hover and while dragging.
          "absolute inset-y-0 right-0 z-lift w-2 translate-x-full touch-none cursor-col-resize select-none rounded-full after:pointer-events-none after:absolute after:left-1/2 after:top-1/2 after:h-10 after:w-1 after:-translate-x-1/2 after:-translate-y-1/2 after:rounded-full after:bg-transparent after:opacity-0 after:transition-[opacity,height,background-color] after:duration-150 after:content-[''] hover:after:h-14 hover:after:bg-ink-2 hover:after:opacity-100 data-[dragging]:after:h-14 data-[dragging]:after:bg-ink-2 data-[dragging]:after:opacity-100 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus motion-reduce:after:transition-none max-narrow:hidden"
        }
        onPointerDown={(e) => {
          if (e.button !== 0) return
          e.preventDefault()
          setDragging(true)
          e.currentTarget.setPointerCapture(e.pointerId)
          drag.current = { x: e.clientX, width: ref.current!.getBoundingClientRect().width }
        }}
        onPointerMove={(e) => {
          if (drag.current) apply(drag.current.width + e.clientX - drag.current.x)
        }}
        onPointerUp={(e) => {
          setDragging(false)
          if (!drag.current) return
          save(width)
          drag.current = null
          e.currentTarget.releasePointerCapture(e.pointerId)
        }}
        onPointerCancel={() => {
          drag.current = null
          setDragging(false)
        }}
        onDoubleClick={() => save(apply(SIDEBAR_WIDTH))}
        onKeyDown={(e) => {
          if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(e.key)) return
          e.preventDefault()
          save(apply(e.key === 'Home' ? MIN : e.key === 'End' ? MAX : width + (e.key === 'ArrowRight' ? 16 : -16)))
        }}
      />
    </nav>
  )
}
