import { Button as ButtonPrimitive } from "@base-ui/react/button";
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useRef,
  useState,
  type ComponentProps,
  type ReactNode,
  type Ref,
} from "react";
import {
  DialogPresence,
  isTopDialog,
  useDialogMotion,
  type DialogOrigin,
} from "../../boardui/overlays/dialog-motion";
import {
  DIALOG_BACKDROP,
  DIALOG_CONTENT,
  DIALOG_HEADER,
  DIALOG_SURFACE,
  DIALOG_VIEWPORT,
} from "../../boardui/overlays/dialog-styles";
import { cn } from "../../lib/utils";
import { classes } from "../../settings/recipes";

/**
 * shadcn/ui Dialog (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft and kept on the one dialog lifecycle
 * (boardui/overlays/dialog-motion.tsx).
 *
 * Same parts as shadcn: `Dialog`, `DialogTrigger`, `DialogContent` (which
 * composes `DialogPortal`, `DialogOverlay` and the popup), `DialogHeader`,
 * `DialogTitle`, `DialogDescription`, `DialogFooter`, `DialogClose`.
 *
 *   <Dialog open={open} onOpenChange={setOpen}>
 *     <DialogContent className="w-[480px]">
 *       <DialogHeader><DialogTitle>Rename</DialogTitle></DialogHeader>
 *       ...
 *     </DialogContent>
 *   </Dialog>
 *
 * Where it departs from shadcn, and why (the contract is in
 * packages/ui/AGENTS.md):
 *
 * - Base UI would unmount the popup when `open` turns false and wait for the
 *   CSS transition it can see. Here the popup stays mounted and Base UI stays
 *   open until the shared exit has finished: `DialogContent` is a
 *   `DialogPresence` that keeps the last rendered content through every close
 *   path (X, Cancel, Escape, the scrim, submit, an owner that clears its
 *   state), so focus containment and scroll lock hold through the 120ms exit,
 *   a parent dialog stays under a nested one, and reopening during the exit
 *   re-enters the same popup with no stale completion. `useDialogMotion`
 *   plays the motion (opacity/scale only, 200ms in and 120ms out, the
 *   `growFrom` origin, none under reduced motion) and releases the content
 *   when it ends.
 * - `Dialog` holds the open state itself (controlled by `open`, or its own
 *   from `defaultOpen`), so `DialogTrigger` sits outside Base UI's root and a
 *   closed dialog renders nothing.
 * - Escape closes only the dialog on top (also over a shell that is not a
 *   Base UI dialog), and focus moving out never closes it.
 *
 * A dialog that its owner renders only while something is selected goes in a
 * `DialogPresence` the owner places outside that conditional, as any shell
 * does.
 */
type DialogReason =
  | "trigger-press"
  | "outside-press"
  | "escape-key"
  | "close-press"
  | "imperative-action"
  | "none";

/** What `onOpenChange` is told about a change; `cancel()` keeps the dialog as it is. */
export interface DialogChangeDetails {
  reason: DialogReason;
  event: Event;
  cancel: () => void;
  readonly isCanceled: boolean;
}

interface DialogContextValue {
  open: boolean;
  modal: boolean | "trap-focus";
  disablePointerDismissal: boolean;
  change: (open: boolean, details: DialogChangeDetails) => void;
}

const DialogContext = createContext<DialogContextValue | null>(null);

function useDialogContext() {
  const context = useContext(DialogContext);
  if (!context) throw new Error("Dialog parts must sit inside <Dialog>.");
  return context;
}

export type DialogProps = {
  open?: boolean | undefined;
  /** Whether it starts open, when `open` is not given. */
  defaultOpen?: boolean | undefined;
  onOpenChange?:
    | ((open: boolean, details: DialogChangeDetails) => void)
    | undefined;
  /** Focus is trapped and the page scroll locked; `false` leaves the page usable. */
  modal?: boolean | "trap-focus" | undefined;
  /** The scrim does not dismiss. Escape still does. */
  disablePointerDismissal?: boolean | undefined;
  children?: ReactNode;
};

function Dialog({
  open: controlled,
  defaultOpen = false,
  onOpenChange,
  modal = true,
  disablePointerDismissal = false,
  children,
}: DialogProps) {
  const [own, setOwn] = useState(defaultOpen);
  const open = controlled ?? own;
  const isControlled = controlled !== undefined;
  // The latest handler, whichever render an obsolete exit completes in.
  const handler = useRef(onOpenChange);
  handler.current = onOpenChange;
  const change = useCallback(
    (next: boolean, details: DialogChangeDetails) => {
      handler.current?.(next, details);
      if (!details.isCanceled && !isControlled) setOwn(next);
    },
    [isControlled],
  );
  return (
    <DialogContext value={{ open, modal, disablePointerDismissal, change }}>
      {children}
    </DialogContext>
  );
}

function DialogTrigger({
  onClick,
  ...props
}: Omit<ComponentProps<typeof ButtonPrimitive>, "type">) {
  const { open, change } = useDialogContext();
  return (
    <ButtonPrimitive
      data-slot="dialog-trigger"
      aria-haspopup="dialog"
      aria-expanded={open}
      onClick={(event) => {
        onClick?.(event);
        if (event.defaultPrevented) return;
        let canceled = false;
        change(true, {
          reason: "trigger-press",
          event: event.nativeEvent,
          cancel: () => {
            canceled = true;
          },
          get isCanceled() {
            return canceled;
          },
        });
      }}
      {...props}
    />
  );
}

function DialogPortal(props: DialogPrimitive.Portal.Props) {
  return <DialogPrimitive.Portal data-slot="dialog-portal" {...props} />;
}

function DialogClose(props: DialogPrimitive.Close.Props) {
  return <DialogPrimitive.Close data-slot="dialog-close" {...props} />;
}

/** The scrim. Pressing it dismisses; `forceRender` keeps a nested dialog's own scrim, as stacked dialogs always had. */
function DialogOverlay({
  className,
  ...props
}: DialogPrimitive.Backdrop.Props) {
  return (
    <DialogPrimitive.Backdrop
      data-slot="dialog-overlay"
      forceRender
      className={cn(DIALOG_BACKDROP, className)}
      {...props}
    />
  );
}

export type DialogContentProps = Omit<
  DialogPrimitive.Popup.Props,
  "className" | "children"
> & {
  /** The surface: its size, overflow and layout. */
  className?: string | undefined;
  children?: ReactNode;
  /** `cover` over a pane (a dialog), `top` for what nothing may cover (a viewer). */
  layer?: "cover" | "top";
  /** The scrim's fill, e.g. the window's own ground for a dialog that is its window. */
  scrim?: string;
  /** Added to the frame the surface is centred in (its padding, alignment). */
  viewportClassName?: string;
  /** What the dialog grows out of and shrinks back into; see `DialogOrigin`. */
  growFrom?: DialogOrigin | undefined;
  /** The surface element, once it is mounted (it mounts after the dialog's first commit). */
  ref?: Ref<HTMLDivElement> | undefined;
};

const layers = { cover: "z-cover", top: "z-top" } as const;

function DialogContent({ children, ...props }: DialogContentProps) {
  const { open } = useDialogContext();
  return (
    <DialogPresence>
      {open && <DialogSurface {...props}>{children}</DialogSurface>}
    </DialogPresence>
  );
}

/** Mounted from the dialog opening until its exit has ended: Base UI is open throughout. */
function DialogSurface({
  className,
  children,
  layer = "cover",
  scrim = "bg-scrim",
  viewportClassName,
  growFrom,
  ref,
  finalFocus,
  ...props
}: DialogContentProps) {
  const context = useDialogContext();
  // Where focus was when the dialog opened: it goes back there once the exit
  // has ended, unless a newer dialog already holds it.
  const [opener] = useState(() =>
    typeof document === "undefined" ? null : document.activeElement,
  );
  const panelRef = useRef<HTMLDivElement | null>(null);
  const returnFocus = useCallback(() => {
    if (!opener?.isConnected || opener === document.body) return true;
    const active = document.activeElement;
    if (
      active &&
      active !== document.body &&
      !panelRef.current?.contains(active) &&
      active.closest('[role="dialog"]')
    )
      return false;
    return opener as HTMLElement;
  }, [opener]);
  // Base UI mounts the portal's content after this component's first commit,
  // so the motion is handed the elements once they exist (state, not refs).
  const [overlayElement, setOverlayElement] = useState<HTMLDivElement | null>(
    null,
  );
  const [panelElement, setPanelElement] = useState<HTMLDivElement | null>(null);
  const setPanel = useCallback(
    (element: HTMLDivElement | null) => {
      panelRef.current = element;
      setPanelElement(element);
      if (typeof ref === "function") ref(element);
      else if (ref) ref.current = element;
    },
    [ref],
  );
  const overlay = useMemo(
    () => ({ current: overlayElement }),
    [overlayElement],
  );
  const panel = useMemo(() => ({ current: panelElement }), [panelElement]);
  useDialogMotion(panel, overlay, growFrom);
  return (
    <DialogPrimitive.Root
      open
      modal={context.modal}
      disablePointerDismissal={context.disablePointerDismissal}
      onOpenChange={(next, details) => {
        if (next) return;
        // Moving focus away never closes; Escape closes the top dialog only.
        if (details.reason === "focus-out") return details.cancel();
        if (
          details.reason === "escape-key" &&
          (details.event.defaultPrevented || !isTopDialog(panelElement))
        )
          return details.cancel();
        context.change(false, details as DialogChangeDetails);
      }}
    >
      <DialogPortal>
        <DialogOverlay
          ref={setOverlayElement}
          className={cn(layers[layer], scrim)}
        />
        <div
          data-slot="dialog-viewport"
          className={cn(DIALOG_VIEWPORT, layers[layer], viewportClassName)}
        >
          <DialogPrimitive.Popup
            ref={setPanel}
            data-slot="dialog-content"
            aria-modal="true"
            finalFocus={finalFocus ?? returnFocus}
            className={cn(
              DIALOG_SURFACE,
              "pointer-events-auto max-h-full max-w-full",
              className,
            )}
            {...props}
          >
            {children}
          </DialogPrimitive.Popup>
        </div>
      </DialogPortal>
    </DialogPrimitive.Root>
  );
}

/** The title row: the title, and the close beside it. */
function DialogHeader({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-header"
      className={cn(DIALOG_HEADER, className)}
      {...props}
    />
  );
}

/** The dialog's body, padded as the header is. */
function DialogBody({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-body"
      className={cn(DIALOG_CONTENT, className)}
      {...props}
    />
  );
}

/** Right-aligned actions, pinned while the body scrolls. */
function DialogFooter({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-footer"
      className={cn(classes("modal-footer"), className)}
      {...props}
    />
  );
}

function DialogTitle({ className, ...props }: DialogPrimitive.Title.Props) {
  return (
    <DialogPrimitive.Title
      data-slot="dialog-title"
      className={cn("min-w-0 truncate text-title-2-semibold", className)}
      {...props}
    />
  );
}

function DialogDescription({
  className,
  ...props
}: DialogPrimitive.Description.Props) {
  return (
    <DialogPrimitive.Description
      data-slot="dialog-description"
      className={cn(
        "px-6 pb-2 text-body-regular text-ink-2 max-narrow:px-4",
        className,
      )}
      {...props}
    />
  );
}

export {
  Dialog,
  DialogBody,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogOverlay,
  DialogPortal,
  DialogTitle,
  DialogTrigger,
};
