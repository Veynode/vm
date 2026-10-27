import { useId } from 'react'
import { STICKER_OFF, STICKERS, stickerPaints } from '@console/components/computer/computer-stickers'
import type { ComputerIconColor, ComputerIconKind } from '@console/components/computer/computer-icon'
import { StickerArt } from '@console/components/ui/Sticker'
import { useResolvedTheme } from '@console/hooks/useResolvedTheme'
import { avatarIcons, lookColors } from '@veynode/shared/avatar'

/**
 * Core's computer icon (ds 电脑图标, 贴纸), drawn as console's
 * `ComputerDrawing` draws it: a device of `kind` in `color` (the kind's own
 * colour when neutral), grey while `off`. Decorative: the row names the thing.
 */
export function Sticker({ kind, color = 'neutral', off = false, size = 32 }: { kind: ComputerIconKind; color?: ComputerIconColor; off?: boolean; size?: number }) {
  const theme = useResolvedTheme()
  const id = 'ci' + useId().replace(/[^a-zA-Z0-9_-]/g, '')
  const drawing = STICKERS[kind]
  const paint = off ? STICKER_OFF : stickerPaints[color === 'neutral' ? drawing.color : color]
  return (
    <svg className="block shrink-0" width={size} height={size} viewBox="0 0 64 64" overflow="visible" aria-hidden focusable="false" data-computer-icon={kind}>
      <StickerArt id={id} sil={drawing.sil} tilt={drawing.tilt} size={size} dark={theme === 'dark'} dim={off}>
        {drawing.art(paint, { off, work: false })}
      </StickerArt>
    </svg>
  )
}

/** A stored icon, `KIND[:COLOR]` (`veynodevm icon`), read back; anything else is no icon. */
export function parseIcon(icon: string | null | undefined): { kind: ComputerIconKind; color: ComputerIconColor } | null {
  const [kind, color = 'neutral'] = (icon ?? '').split(':')
  const known = avatarIcons.find((k) => k === kind)
  if (!known || (color !== 'neutral' && !lookColors.some((c) => c === color))) return null
  return { kind: known, color: color as ComputerIconColor }
}

/** The kind a container shows while none is chosen: a monitor with a desktop, else core's small host. */
export const containerKind = (desktop: boolean): ComputerIconKind => (desktop ? 'monitor' : 'box')

/** The kind a VM shows while none is chosen. */
export const machineKind: ComputerIconKind = 'vm'

/** A VM's mark: the icon chosen for it, else a virtual machine; grey while it is stopped. */
export function MachineIcon({ icon, up, size }: { icon?: string | null; up: boolean; size?: number }) {
  const chosen = parseIcon(icon)
  return <Sticker kind={chosen?.kind ?? machineKind} color={chosen?.color} off={!up} size={size} />
}

/** A container's mark: the icon chosen for it, else its kind; grey while it is stopped. */
export function ContainerIcon({ icon, desktop, up, size }: { icon?: string | null; desktop: boolean; up: boolean; size?: number }) {
  const chosen = parseIcon(icon)
  return <Sticker kind={chosen?.kind ?? containerKind(desktop)} color={chosen?.color} off={!up} size={size} />
}

/** FNV-1a: a stable number from a name, the same on the server and in the browser. */
function hash(text: string) {
  let h = 0x811c9dc5
  for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 0x01000193)
  return h >>> 0
}

/** An image's outline: three stacked layers seen from above, as one hexagon. */
const STACK = 'M8 20L32 8L56 20V46L32 58L8 46Z'

/** One layer, its top face centred at (32, `y`): the top, then its two sides. */
function Layer({ y, top, side, dark }: { y: number; top: string; side: string; dark: string }) {
  return (
    <>
      <path d={`M8 ${y}L32 ${y + 12}V${y + 18}L8 ${y + 6}Z`} fill={side} />
      <path d={`M56 ${y}L32 ${y + 12}V${y + 18}L56 ${y + 6}Z`} fill={dark} />
      <path d={`M8 ${y}L32 ${y - 12}L56 ${y}L32 ${y + 12}Z`} fill={top} />
    </>
  )
}

/** The marks an image's top layer can carry, drawn flat and laid on the layer. */
const MARKS = [
  <circle key="dot" cx={0} cy={0} r={5} />,
  <rect key="square" x={-5} y={-5} width={10} height={10} rx={2} />,
  <path key="ring" d="M0-6a6 6 0 1 1 0 12a6 6 0 1 1 0-12Zm0 3a3 3 0 1 0 0 6a3 3 0 1 0 0-6Z" fillRule="evenodd" />,
  <path key="plus" d="M-1.6-6h3.2v4.4H6v3.2H1.6V6h-3.2V1.6H-6v-3.2h4.4Z" />,
]

/**
 * The layers' colours from `h`, bottom first: one in three images is one
 * colour throughout, the others three different ones.
 */
function layerColors(h: number) {
  if ((h >>> 24) % 3 === 0) return Array(3).fill(stickerPaints[lookColors[h % lookColors.length]!])
  const picked: number[] = []
  for (let i = 0, rest = h; picked.length < 3; i++, rest = Math.floor(rest / lookColors.length) + i) {
    const c = rest % lookColors.length
    if (!picked.includes(c)) picked.push(c)
  }
  return picked.map((c) => stickerPaints[lookColors[c]!])
}

/** The sticker's tilt from `h`: one way or the other, never flat. */
const TILTS = [-6, -4, -3, 3, 4, 6]

/**
 * An image's mark: a sticker of its layers, not a device, so an image is not
 * taken for a container or a VM. Each layer's colour, the mark on the top one
 * and the sticker's tilt are picked from its name (repository, without the
 * registry and tag), so every tag of one image looks alike and the same name
 * always draws the same.
 */
export function ImageIcon({ reference, size = 32 }: { reference: string; size?: number }) {
  const theme = useResolvedTheme()
  const id = 'im' + useId().replace(/[^a-zA-Z0-9_-]/g, '')
  const name = reference.replace(/^[^/]+\.[^/]+\//, '').replace(/^library\//, '').replace(/[:@].*$/, '')
  const h = hash(name)
  const [bottom, middle, top] = layerColors(h)
  // A paint is [main, side, light, ink]: the light top face, the main and side walls.
  return (
    <svg className="block shrink-0" width={size} height={size} viewBox="0 0 64 64" overflow="visible" aria-hidden focusable="false" data-image-icon="">
      <StickerArt id={id} sil={STACK} tilt={TILTS[(h >>> 20) % TILTS.length]!} size={size} dark={theme === 'dark'}>
        <path d={STACK} fill={bottom[1]} />
        <Layer y={40} top={bottom[2]} side={bottom[0]} dark={bottom[1]} />
        <Layer y={30} top={middle[2]} side={middle[0]} dark={middle[1]} />
        {/* The top face in its main colour, for the mark to stand out on. */}
        <Layer y={20} top={top[0]} side={top[1]} dark={top[1]} />
        {/* The layer's face is a square seen at 30°: lay the mark on it. */}
        <g transform="matrix(1 .5 -1 .5 32 20)" fill={top[3]} opacity={0.92}>
          {MARKS[(h >>> 8) % MARKS.length]}
        </g>
      </StickerArt>
    </svg>
  )
}
