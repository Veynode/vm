import { useState } from 'react'
import { useRouter } from '@tanstack/react-router'
import type { ComputerIconColor, ComputerIconKind } from '@console/components/computer/computer-icon'
import { stickerPaints } from '@console/components/computer/computer-stickers'
import { Button } from '@ui/components/ui/button'
import { CloseButton } from '@ui/components/ui/close-button'
import { Dialog, DialogClose, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@ui/components/ui/dialog'
import { classes } from '@ui/settings/recipes'
import { avatarIcons, lookColors } from '@veynode/shared/avatar'
import { setContainerIcon, setMachineIcon, type ContainerRow, type MachineRecord } from './api'
import { Sticker, containerKind, machineKind, parseIcon } from './container-icon'
import { useToastAction } from './ui'

const KIND_NAMES: Record<ComputerIconKind, string> = {
  monitor: 'Monitor',
  laptop: 'Laptop',
  vm: 'Virtual machine',
  box: 'Container',
  server: 'Server',
  terminal: 'Terminal',
  cpu: 'Chip',
  globe: 'Globe',
  folder: 'Folder',
  code: 'Code',
}
const COLORS: readonly ComputerIconColor[] = ['neutral', ...lookColors]

/** A swatch as core's picker draws it: the colour's own gradient, Auto struck through. */
const swatch = (color: ComputerIconColor) =>
  color === 'neutral'
    ? 'linear-gradient(135deg,transparent 46%,var(--color-line-strong) 46%,var(--color-line-strong) 54%,transparent 54%)'
    : `linear-gradient(135deg,${stickerPaints[color][0]},${stickerPaints[color][1]})`

/** What an icon is chosen for: `key` tells one opening from the next, `fallback` is drawn while none is chosen. */
type Target = { key: string; name: string; icon?: string | null; fallback: ComputerIconKind }

/** A container's icon, saved with `veynodevm icon`. */
export function IconDialog({ container, close }: { container: Pick<ContainerRow, 'id' | 'name' | 'desktop' | 'icon'> | null; close: () => void }) {
  return (
    <StickerDialog
      target={container && { key: container.id, name: container.name, icon: container.icon, fallback: containerKind(container.desktop) }}
      save={(icon) => setContainerIcon({ data: { id: container!.id, icon } })}
      close={close}
    />
  )
}

/** A VM's icon, saved with `veynodevm machine icon`. */
export function MachineIconDialog({ machine, close }: { machine: Pick<MachineRecord, 'name' | 'icon'> | null; close: () => void }) {
  return (
    <StickerDialog
      target={machine && { key: machine.name, name: machine.name, icon: machine.icon, fallback: machineKind }}
      save={(icon) => setMachineIcon({ data: { name: machine!.name, icon } })}
      close={close}
    />
  )
}

/**
 * Choosing an icon (core's 电脑图标 · 选图标, without the upload): a large
 * preview, the ten devices drawn in the chosen colour, then Auto and the
 * eight colours. Reset goes back to the fallback. Saved through the CLI, so
 * the CLI and every browser see it.
 */
function StickerDialog({ target, save: write, close }: { target: Target | null; save: (icon: string | null) => Promise<unknown>; close: () => void }) {
  const router = useRouter()
  const stored = parseIcon(target?.icon)
  const [choice, setChoice] = useState<{ kind: ComputerIconKind | null; color: ComputerIconColor } | null>(null)
  // Each opening starts from what is stored.
  const [openedFor, setOpenedFor] = useState<string | null>(null)
  if ((target?.key ?? null) !== openedFor) {
    setOpenedFor(target?.key ?? null)
    setChoice(stored ? { ...stored } : { kind: null, color: 'neutral' })
  }
  const fallback = target?.fallback ?? 'box'
  const kind = choice?.kind ?? null
  const color = choice?.color ?? 'neutral'
  const save = useToastAction(async (icon: string | null) => {
    if (!target) return
    await write(icon)
    await router.invalidate()
    close()
  })
  const value = kind ? (color === 'neutral' ? kind : `${kind}:${color}`) : color === 'neutral' ? null : `${fallback}:${color}`
  return (
    <Dialog open={target !== null} onOpenChange={(next) => !next && !save.pending && close()}>
      <DialogContent className={classes('modal sm')}>
        <DialogHeader className="modal-head">
          <DialogTitle>{`Icon for ${target?.name ?? ''}`}</DialogTitle>
          <DialogClose render={<CloseButton size="sm" aria-label="Close" disabled={save.pending} />} />
        </DialogHeader>
        <div className={classes('dialog-body')}>
          <div className="grid h-28 place-items-center rounded-lg bg-[radial-gradient(120%_90%_at_50%_0%,var(--color-surface)_0%,var(--color-surface-2)_70%)] inset-ring inset-ring-line">
            <Sticker kind={kind ?? fallback} color={color} size={72} />
          </div>
          <div role="group" aria-label="Icon" className="grid grid-cols-5 gap-1">
            {avatarIcons.map((glyph) => (
              <button
                key={glyph}
                type="button"
                aria-pressed={kind === glyph}
                aria-label={KIND_NAMES[glyph]}
                title={KIND_NAMES[glyph]}
                onClick={() => setChoice({ kind: glyph, color })}
                className={
                  'grid h-14 place-items-center rounded-md outline-none transition-[background-color,transform] duration-[var(--motion-slow)] ease-pop hover:bg-surface-2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus active:scale-[.94] motion-reduce:transition-none motion-reduce:active:scale-100 ' +
                  (kind === glyph ? 'bg-surface-2 inset-ring-2 inset-ring-ink' : '')
                }
              >
                <Sticker kind={glyph} color={color} size={36} />
              </button>
            ))}
          </div>
          <div role="group" aria-label="Colour" className="grid grid-cols-[repeat(9,26px)] justify-between gap-0.5 px-0.5 py-1">
            {COLORS.map((c) => (
              <button
                key={c}
                type="button"
                aria-pressed={color === c}
                aria-label={c === 'neutral' ? 'Auto colour' : `${c[0]!.toUpperCase()}${c.slice(1)}`}
                title={c === 'neutral' ? 'Auto' : `${c[0]!.toUpperCase()}${c.slice(1)}`}
                onClick={() => setChoice({ kind, color: c })}
                className={
                  'relative grid size-7 place-items-center rounded-sm outline-none transition-shadow duration-[var(--motion-fast)] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus motion-reduce:transition-none ' +
                  (color === c ? 'ring-2 ring-ink ring-offset-[1.5px] ring-offset-surface' : '')
                }
              >
                <span
                  aria-hidden
                  className="block size-5"
                  style={{ borderRadius: 4, backgroundImage: swatch(c), boxShadow: '0 0 0 2.5px #fff, 0 0 0 3.5px var(--color-line-strong)' }}
                />
              </button>
            ))}
          </div>
        </div>
        <DialogFooter className="justify-between">
          <Button variant="ghost" disabled={save.pending || !stored} onClick={() => save.run(null)}>
            Reset
          </Button>
          <div className="flex gap-2">
            <Button variant="outline" disabled={save.pending} onClick={close}>
              Cancel
            </Button>
            <Button pending={save.pending} onClick={() => save.run(value)}>
              Save
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
