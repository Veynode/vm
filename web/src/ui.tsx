// VeynodeVM's page primitives, drawn as core's console draws its own
// (packages/console/src/components/ui/): the pages stand in core's SettingsPage
// shell and are made of its groups and rows (src/vendor/veynode-ui/settings).
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react'
import { createPortal } from 'react-dom'
import { useRouter } from '@tanstack/react-router'
import { AnimatedIcon } from '@ui/boardui/icons/animated-icon'
import { Button } from '@ui/components/ui/button'
import { CloseButton } from '@ui/components/ui/close-button'
import { Switch, type SwitchProps } from '@ui/components/ui/switch'
import { Dialog, DialogBody, DialogClose, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@ui/components/ui/dialog'
import { Toast, type ToastVariant } from '@ui/components/ui/toast'
import { CircleAlert, type LucideIcon } from '@ui/icons'
import { cn } from '@ui/lib/utils'
import { InfoTip } from '@ui/settings/info-tip'
import { classes } from '@ui/settings/recipes'
import { SettingsActionsSlot } from '@ui/settings/settings-page'
import { StateTag, type StateTone } from '@ui/settings/state-tag'

/** Re-run route loaders every `ms` while the page is visible. */
export function usePoll(ms: number) {
  const router = useRouter()
  useEffect(() => {
    const timer = setInterval(() => {
      if (document.visibilityState === 'visible') router.invalidate()
    }, ms)
    return () => clearInterval(timer)
  }, [router, ms])
}

/** Wrap an async action with pending and error state. */
export function useAction<A extends unknown[], R>(fn: (...args: A) => Promise<R>) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const run = async (...args: A): Promise<R | undefined> => {
    setPending(true)
    setError(null)
    try {
      return await fn(...args)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      return undefined
    } finally {
      setPending(false)
    }
  }
  return { run, pending, error }
}

export function message(e: unknown) {
  return e instanceof Error ? e.message : String(e)
}

/* ----- Page shell ---------------------------------------------------------- */

/** A page under a place (a container, its terminal): its title and the way one level up. */
export type SubPage = { heading: ReactNode; back: () => void }

export const SubPageContext = createContext<(page: SubPage | null) => void>(() => {})

/** Names this page in the title band and gives it a Back; the place's own name and no Back once it unmounts. */
export function useSubPage(heading: ReactNode, back: () => void) {
  const set = useContext(SubPageContext)
  const latest = useRef(back)
  latest.current = back
  useEffect(() => {
    set({ heading, back: () => latest.current() })
  }, [set, heading])
  useEffect(() => () => set(null), [set])
}

/** A page's root: its parts 32px apart in body text (`settings-page-body`). */
export function PageBody({ label, className, children }: { label: string; className?: string; children: ReactNode }) {
  return (
    <section aria-label={label} className={cn(classes('settings-page-body'), className)}>
      {children}
    </section>
  )
}

/** The page's actions, put in the title band at its end (core's `PanelHeader`). */
export function PageActions({ children }: { children: ReactNode }) {
  const slot = useContext(SettingsActionsSlot)
  return slot ? createPortal(children, slot) : null
}

/* ----- States -------------------------------------------------------------- */

/** A state's tone; `fact` is a word that is no state (a kind, a count). */
export type Tone = StateTone | 'fact'

/** A container's or the engine's state as one `StateTag`. */
export function StatusTag({ state, status }: { state: string; status: string }) {
  const up = !['stopped', 'crashed', 'missing', 'unknown', 'absent'].includes(state)
  const bad = state === 'crashed' || status.startsWith('Failed') || /Exited \((?!0\))/.test(status)
  const busy = /^(starting|creating|stopping|booting|pulling)/i.test(state)
  const tone: StateTone = bad ? 'failed' : busy ? 'working' : up ? 'done' : 'neutral'
  return (
    <StateTag tone={tone} busy={busy} title={status}>
      {status}
    </StateTag>
  )
}

/**
 * Confirmed absence: no box, one line and at most one button, in the 208px
 * frame `PanelLoadError` shares (core's `EmptyState`).
 */
export function EmptyState({ icon: Icon, title, action }: { icon?: LucideIcon; title: string; action?: ReactNode }) {
  return (
    <div data-empty-state="" className="flex min-h-52 flex-col items-center justify-center gap-4 px-4 py-8 text-center">
      {Icon && <Icon className="size-7 text-ink-3" aria-hidden />}
      <p className="text-chat-regular text-ink-2">{title}</p>
      {action && <div>{action}</div>}
    </div>
  )
}

/** The page's data did not load: one sentence and Retry in the empty state's frame, the raw reason behind its ⓘ. */
export function PanelLoadError({ title, detail, onRetry }: { title: string; detail?: string; onRetry: () => unknown }) {
  const [busy, setBusy] = useState(false)
  return (
    <div role="alert" className="flex min-h-52 flex-col items-center justify-center gap-4 px-4 py-8 text-center">
      <CircleAlert className="size-7 text-ink-3" aria-hidden />
      <p className="flex items-center gap-1 text-chat-regular text-ink-2">
        {title}
        {detail && <InfoTip label="What happened">{detail}</InfoTip>}
      </p>
      <Button
        variant="secondary"
        pending={busy}
        onClick={async () => {
          setBusy(true)
          try {
            await onRetry()
          } finally {
            setBusy(false)
          }
        }}
      >
        Retry
      </Button>
    </div>
  )
}

/* ----- Dialogs and toasts -------------------------------------------------- */

/**
 * The one confirmation before a hard-to-reverse action (DESIGN.md "Dialog
 * anatomy"): a question as the title, the consequence, then Cancel and the
 * specific verb. While `busy` it cannot be dismissed.
 */
export function ConfirmDialog({
  open,
  title,
  children,
  confirmLabel,
  confirmVariant = 'destructive',
  busy = false,
  onConfirm,
  close,
}: {
  open: boolean
  title: string
  children?: ReactNode
  confirmLabel: string
  /** `default` for a costly but harmless step (a large download). */
  confirmVariant?: 'destructive' | 'default'
  busy?: boolean
  onConfirm: () => void
  close: () => void
}) {
  return (
    <Dialog open={open} onOpenChange={(next) => !next && !busy && close()}>
      <DialogContent role="alertdialog" className={classes('modal sm')}>
        <DialogHeader className="modal-head">
          <DialogTitle>{title}</DialogTitle>
          <DialogClose render={<CloseButton size="sm" aria-label="Close" disabled={busy} />} />
        </DialogHeader>
        {children && <DialogBody className="text-body-regular text-ink-2 [overflow-wrap:anywhere]">{children}</DialogBody>}
        <DialogFooter>
          <Button variant="outline" disabled={busy} onClick={close}>
            Cancel
          </Button>
          <Button variant={confirmVariant} pending={busy} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/**
 * A form in a dialog (core's `modal` with an `entity-form`): the title and
 * close, the fields scrolling in between, Cancel and the submit verb pinned
 * at the foot. Creating something opens one; a failed submit is a toast and
 * the form keeps what was typed. While `busy` it cannot be dismissed.
 */
export function FormDialog({
  open,
  close,
  title,
  size = 'sm',
  busy = false,
  submitLabel,
  canSubmit = true,
  onSubmit,
  children,
}: {
  open: boolean
  close: () => void
  title: string
  size?: 'sm' | 'md'
  busy?: boolean
  submitLabel: string
  canSubmit?: boolean
  onSubmit: () => void
  children: ReactNode
}) {
  return (
    <Dialog open={open} onOpenChange={(next) => !next && !busy && close()}>
      <DialogContent className={classes(size === 'sm' ? 'modal sm' : 'modal')}>
        <DialogHeader className="modal-head">
          <DialogTitle>{title}</DialogTitle>
          <DialogClose render={<CloseButton size="sm" aria-label="Close" disabled={busy} />} />
        </DialogHeader>
        <form
          className={cn(classes('entity-form'), 'flex-1')}
          onSubmit={(e) => {
            e.preventDefault()
            if (canSubmit && !busy) onSubmit()
          }}
        >
          <div className={cn(classes('entity-form-body'), '*:shrink-0')}>{children}</div>
          <DialogFooter>
            <Button variant="outline" disabled={busy} onClick={close}>
              Cancel
            </Button>
            <Button type="submit" pending={busy} disabled={!canSubmit}>
              {submitLabel}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

type Notice = { id: number; message: string; variant: ToastVariant }

const ToastContext = createContext<(message: string, variant?: ToastVariant) => void>(() => {})

/**
 * One toast at a time at the bottom centre (core's `ToastProvider`): a
 * one-off action that failed, or one that worked where its result is not on
 * screen. An error stays 6s, anything else 3.5s.
 */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [notice, setNotice] = useState<Notice | null>(null)
  const next = useRef(0)
  const show = useCallback((message: string, variant: ToastVariant = 'success') => {
    setNotice({ id: ++next.current, message, variant })
  }, [])
  return (
    <ToastContext value={show}>
      {children}
      {notice && (
        <Toast
          key={notice.id}
          message={notice.message}
          variant={notice.variant}
          duration={notice.variant === 'error' ? 6000 : 3500}
          dismissLabel="Dismiss"
          onDismiss={() => setNotice((current) => (current?.id === notice.id ? null : current))}
        />
      )}
    </ToastContext>
  )
}

export function useToast() {
  return useContext(ToastContext)
}

/** Run a one-off action; its failure is a toast (DESIGN.md "Error placement"). */
export function useToastAction<A extends unknown[], R>(fn: (...args: A) => Promise<R>) {
  const toast = useToast()
  const [pending, setPending] = useState(false)
  const run = async (...args: A): Promise<R | undefined> => {
    setPending(true)
    try {
      return await fn(...args)
    } catch (e) {
      toast(message(e), 'error')
      return undefined
    } finally {
      setPending(false)
    }
  }
  return { run, pending }
}

/* ----- Content ------------------------------------------------------------- */

/** Commands, logs and configuration: mono text on the veil, scrolling past 420px. */
export function CodeBlock({ children, className, ref }: { children: ReactNode; className?: string; ref?: React.Ref<HTMLPreElement> }) {
  return (
    <pre ref={ref} className={cn('max-h-[420px] overflow-auto rounded-lg bg-surface-2 px-4 py-3 font-mono text-[13px] leading-5 break-words whitespace-pre-wrap text-ink', className)}>
      {children}
    </pre>
  )
}

export function age(seconds: number) {
  const d = Math.max(0, Date.now() / 1000 - seconds)
  if (d < 60) return `${Math.floor(d)}s ago`
  if (d < 3600) return `${Math.floor(d / 60)}m ago`
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`
  return `${Math.floor(d / 86400)}d ago`
}

export function size(bytes: number) {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(2)} GB`
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MB`
  return `${(bytes / 1024).toFixed(1)} kB`
}

/** Split a command line into argv, honouring single and double quotes. */
export function words(line: string): string[] {
  const out: string[] = []
  let current = ''
  let quote: string | null = null
  let started = false
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null
      else current += ch
    } else if (ch === '"' || ch === "'") {
      quote = ch
      started = true
    } else if (/\s/.test(ch)) {
      if (started || current) out.push(current)
      current = ''
      started = false
    } else {
      current += ch
    }
  }
  if (quote) throw new Error('unterminated quote')
  if (started || current) out.push(current)
  return out
}

export function lines(text: string): string[] {
  return text.split('\n').map((s) => s.trim()).filter(Boolean)
}

/**
 * Core's `Switch` (md) while what it turns on or off is still happening:
 * `pending` holds it at `checked`, the state it is going to, with a spinner
 * on its thumb, as a pending `Button` keeps its place with one. It is drawn
 * as it will be, not dimmed as disabled, and takes no clicks or keys.
 */
export function PendingSwitch({ pending, checked, onCheckedChange, ...props }: Omit<SwitchProps, 'size'> & { pending: boolean; checked: boolean }) {
  return (
    <span className={cn('relative inline-flex', pending && 'pointer-events-none')} {...(pending ? { 'aria-busy': true } : {})}>
      <Switch {...props} checked={checked} onCheckedChange={(next, details) => !pending && onCheckedChange?.(next, details)} />
      {pending && (
        // Over the md thumb: 16px, 3px in, 16px further along when on.
        <span
          aria-hidden
          className={cn('pointer-events-none absolute top-[3px] grid size-4 place-items-center text-ink transition-[left] duration-200', checked ? 'left-[19px]' : 'left-[3px]')}
        >
          <AnimatedIcon name="loader-circle" working className="size-3" />
        </span>
      )}
    </span>
  )
}
