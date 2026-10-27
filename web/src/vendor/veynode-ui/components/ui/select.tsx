import { Select as SelectPrimitive } from "@base-ui/react/select";
import {
  createContext,
  useContext,
  useMemo,
  useRef,
  type ReactNode,
  type Ref,
  type RefObject,
} from "react";
import { AnimatedIcon } from "../../boardui/icons/animated-icon";
import { Check } from "../../icons";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Select (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: a button and a floating listbox, never a native
 * select.
 *
 *   trigger  the `field-shell` (a `field` fill that turns to `surface` with
 *            the field ring on focus and while open), md radius (sm at `sm`),
 *            36px (28px at `sm`), the value in ink and a chevron in ink-2
 *            that turns while open. `aria-invalid` draws the edge in the
 *            failed colour.
 *   popup    the menu panel (surface, lg radius, `float` shadow, a 1px
 *            hairline), at least as wide as the trigger and 200px, growing to
 *            its longest option up to 360px. It opens in 140ms from the
 *            trigger (up 4px, from .97) and fades out in 100ms; reduced
 *            motion shows and hides it at once.
 *   rows     36px with sm corners: the label (a lead icon or logo may stand
 *            before it), optional `meta` (grey text or a `SelectBadge`) and a
 *            check on the chosen one. `SelectGroup` + `SelectLabel` put rows
 *            under a small grey heading.
 *
 * Give `Select` its `items` (value to label) so the trigger can say the
 * chosen label while the list is closed; `size` on `Select` sets the
 * trigger and the rows. Inside a native dialog or popover the popup is
 * portalled into it, because everything outside a modal dialog is inert.
 */
export type SelectSize = "sm" | "md";

type SelectContextValue = {
  size: SelectSize;
  /** The trigger, so the popup can find the dialog it stands in. */
  anchor: RefObject<HTMLElement | null>;
};

const SelectContext = createContext<SelectContextValue>({
  size: "md",
  anchor: { current: null },
});

/**
 * Where the popup is portalled: into the native dialog or popover the trigger
 * stands in (the rest of the page is inert while a modal dialog is open, and
 * only the top layer is above it), into the fullscreen element, else the page.
 */
function popupContainer(anchor: HTMLElement | null) {
  if (!anchor) return null;
  const host = anchor.closest<HTMLElement>("dialog[open], [popover]");
  if (host) return host;
  const full = document.fullscreenElement;
  return full instanceof HTMLElement && full.contains(anchor) ? full : null;
}

/**
 * The portal's container as a ref read when the popup mounts, not when the
 * select renders: the dialog the trigger stands in may open after that.
 */
export function usePopupContainer(anchor: RefObject<HTMLElement | null>) {
  return useMemo(
    () => ({
      get current() {
        return popupContainer(anchor.current);
      },
    }),
    [anchor],
  );
}

function SelectRoot<Value, Multiple extends boolean | undefined = false>({
  size = "md",
  // Non-modal: a modal select locks the page scroll (`overflow: hidden` on
  // the root), which yanks the sticky sidebar every time one opens.
  modal = false,
  ...props
}: SelectPrimitive.Root.Props<Value, Multiple> & { size?: SelectSize }) {
  const anchor = useRef<HTMLElement | null>(null);
  const context = useMemo(() => ({ size, anchor }), [size]);
  return (
    <SelectContext value={context}>
      <SelectPrimitive.Root modal={modal} {...props} />
    </SelectContext>
  );
}

function SelectGroup({ className, ...props }: SelectPrimitive.Group.Props) {
  return (
    <SelectPrimitive.Group
      data-slot="select-group"
      className={cn(
        "flex flex-col gap-0.5",
        "not-first:mt-1 not-first:border-t not-first:border-line not-first:pt-1",
        className,
      )}
      {...props}
    />
  );
}

function SelectValue({ className, ...props }: SelectPrimitive.Value.Props) {
  return (
    <SelectPrimitive.Value
      data-slot="select-value"
      className={cn(
        "flex min-w-0 flex-1 items-center gap-2 truncate text-left",
        "group-data-[size=sm]/select:gap-1.5",
        className,
      )}
      {...props}
    />
  );
}

type SelectTriggerProps = Omit<SelectPrimitive.Trigger.Props, "className"> & {
  /** Overrides the size of the `Select` it stands in. */
  size?: SelectSize;
  /** Classes for the button. */
  className?: string | undefined;
  /** Classes for the field shell around it (its width, for one). */
  fieldClassName?: string | undefined;
  ref?: Ref<HTMLButtonElement> | undefined;
};

function SelectTrigger({
  size,
  className,
  fieldClassName,
  children,
  ref,
  ...props
}: SelectTriggerProps) {
  const context = useContext(SelectContext);
  const resolved = size ?? context.size;
  return (
    <div
      data-slot="select-field"
      data-size={resolved}
      className={cn(
        "field-shell group/select relative flex w-full min-w-0 items-center",
        // Open, the focus has moved into the popup: the shell stays lit.
        "has-[[data-popup-open]]:bg-surface! has-[[data-popup-open]]:shadow-field-ring",
        resolved === "sm" ? "h-7 rounded-sm" : "h-9 rounded-md",
        fieldClassName,
      )}
    >
      <SelectPrimitive.Trigger
        ref={(node) => {
          context.anchor.current = node;
          if (typeof ref === "function") ref(node);
          else if (ref) ref.current = node;
        }}
        data-slot="select-trigger"
        data-select-trigger=""
        data-size={resolved}
        className={cn(
          "flex size-full min-w-0 cursor-pointer items-center justify-between bg-transparent text-left text-ink",
          "outline-none select-none disabled:cursor-not-allowed disabled:text-ink-3 data-placeholder:text-ink-3",
          resolved === "sm"
            ? "gap-1.5 pl-3 pr-3 text-body-2-regular"
            : "gap-2 pl-3 pr-3.5 text-body-regular",
          className,
        )}
        {...props}
      >
        {children}
        <SelectPrimitive.Icon
          render={(iconProps, state) => (
            <span {...iconProps} className="flex shrink-0 text-ink-2">
              <AnimatedIcon
                name="chevron-down"
                on={state.open}
                className="size-4 shrink-0"
              />
            </span>
          )}
        />
      </SelectPrimitive.Trigger>
    </div>
  );
}

/** The popup panel; the combobox variant draws the same one. */
export const selectPopupClass = cn(
  "relative isolate flex flex-col overflow-hidden rounded-lg bg-surface p-1.5 text-ink shadow-float inset-ring inset-ring-line outline-none",
  // At least the trigger (and 200px), growing to the longest option up to
  // 360px, never past the viewport.
  "w-max min-w-[min(max(200px,var(--anchor-width)),calc(100vw-2rem))] max-w-[min(max(360px,var(--anchor-width)),calc(100vw-2rem))]",
  // In from the trigger over 140ms, out over 100ms fading only.
  "origin-(--transform-origin) transition-[opacity,translate,scale] duration-[140ms] ease-standard",
  "data-starting-style:-translate-y-1 data-starting-style:scale-[0.97] data-starting-style:opacity-0",
  "data-ending-style:opacity-0 data-ending-style:duration-100 data-ending-style:ease-exit",
  "motion-reduce:transition-none",
);

/** The scrolling list inside the popup. */
export const selectListClass =
  "flex max-h-[min(280px,var(--available-height))] w-full flex-col gap-0.5 overflow-y-auto overscroll-contain outline-none";

/** One row; the combobox variant draws the same one. */
export function selectItemClass(size: SelectSize) {
  return cn(
    "group/option flex min-h-9 w-full cursor-pointer items-center justify-start gap-3 rounded-sm py-1.5 pl-3 pr-3 text-left select-none",
    "max-narrow:pointer-coarse:min-h-11 text-ink outline-none",
    "transition-colors duration-[var(--button-transition-ms)]",
    "data-highlighted:bg-surface-2 data-disabled:cursor-not-allowed data-disabled:opacity-45",
    size === "sm"
      ? "text-body-2-regular data-selected:text-body-2-semibold"
      : "text-body-regular data-selected:text-body-semibold",
  );
}

type SelectContentProps = SelectPrimitive.Popup.Props &
  Pick<
    SelectPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset"
  >;

function SelectContent({
  className,
  children,
  "aria-label": ariaLabel,
  "aria-labelledby": ariaLabelledBy,
  side = "bottom",
  sideOffset = 4,
  align = "start",
  alignOffset = 0,
  ...props
}: SelectContentProps) {
  const { anchor } = useContext(SelectContext);
  const container = usePopupContainer(anchor);
  return (
    <SelectPrimitive.Portal container={container}>
      <SelectPrimitive.Positioner
        side={side}
        sideOffset={sideOffset}
        align={align}
        alignOffset={alignOffset}
        collisionPadding={16}
        // A menu, not the native look: it opens below the trigger rather than
        // over the chosen row.
        alignItemWithTrigger={false}
        // Fixed, so a dialog's scrolling or clipping never cuts it.
        positionMethod="fixed"
        className="isolate z-menu"
      >
        <SelectPrimitive.Popup
          data-slot="select-content"
          className={cn(selectPopupClass, className)}
          {...props}
        >
          {/* The list is the listbox: it takes the select's name. */}
          <SelectPrimitive.List
            className={selectListClass}
            {...(ariaLabel === undefined ? {} : { "aria-label": ariaLabel })}
            {...(ariaLabelledBy === undefined
              ? {}
              : { "aria-labelledby": ariaLabelledBy })}
          >
            {children}
          </SelectPrimitive.List>
        </SelectPrimitive.Popup>
      </SelectPrimitive.Positioner>
    </SelectPrimitive.Portal>
  );
}

/** A small grey heading over a group of rows. */
function SelectLabel({
  className,
  ...props
}: SelectPrimitive.GroupLabel.Props) {
  return (
    <SelectPrimitive.GroupLabel
      data-slot="select-label"
      className={cn(
        "px-3 pt-2 pb-1 text-caption-1-semibold text-ink-2",
        className,
      )}
      {...props}
    />
  );
}

type SelectItemProps = SelectPrimitive.Item.Props & {
  /** Trailing meta: plain grey text, or a `SelectBadge`. Not shown in the trigger. */
  meta?: ReactNode;
};

function SelectItem({ className, children, meta, ...props }: SelectItemProps) {
  const { size } = useContext(SelectContext);
  return (
    <SelectPrimitive.Item
      data-slot="select-item"
      className={cn(selectItemClass(size), className)}
      {...props}
    >
      <SelectPrimitive.ItemText className="flex min-w-0 flex-1 items-center gap-3 truncate">
        {children}
      </SelectPrimitive.ItemText>
      <SelectMeta>{meta}</SelectMeta>
      <SelectCheck>
        <SelectPrimitive.ItemIndicator>
          <Check aria-hidden className="size-4 text-ink" />
        </SelectPrimitive.ItemIndicator>
      </SelectCheck>
    </SelectPrimitive.Item>
  );
}

/** The trailing note of a row; nothing when there is none. */
export function SelectMeta({ children }: { children?: ReactNode }) {
  if (children === undefined || children === null || children === false)
    return null;
  return (
    <span
      data-slot="select-meta"
      className="flex shrink-0 items-center text-caption-1-regular text-ink-2"
    >
      {children}
    </span>
  );
}

/** The 16px place of a row's check: it keeps its room while empty. */
export function SelectCheck({ children }: { children?: ReactNode }) {
  return (
    <span className="flex size-4 shrink-0 items-center justify-center">
      {children}
    </span>
  );
}

function SelectSeparator({
  className,
  ...props
}: SelectPrimitive.Separator.Props) {
  return (
    <SelectPrimitive.Separator
      data-slot="select-separator"
      className={cn("mx-1.5 my-1 h-px shrink-0 bg-line", className)}
      {...props}
    />
  );
}

/**
 * A short note at the end of an option (Default, Offline): the one pill tag
 * (20px, 8px sides, caption medium, a tint of the text colour). A tag carries
 * no state colour, so `tone` is kept for callers and every tone draws the
 * same.
 */
function SelectBadge({
  tone = "neutral",
  children,
}: {
  tone?: "neutral" | "done" | "failed" | "waiting" | "working";
  children: ReactNode;
}) {
  return (
    <span
      data-slot="select-badge"
      data-select-badge={tone}
      className="inline-flex h-5 shrink-0 items-center rounded-full bg-ink/6 px-2 text-caption-1-medium whitespace-nowrap text-ink-2"
    >
      {children}
    </span>
  );
}

export {
  SelectRoot as Select,
  SelectBadge,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
};
