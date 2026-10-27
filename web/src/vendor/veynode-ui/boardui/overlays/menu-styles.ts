/**
 * Shared visual recipe for BoardUI menu surfaces, in Veynode Soft: a surface
 * panel with lg corners and the `float` shadow (the only shadow a menu may
 * have), rows of 36px (`control-md`; 44px on a phone's touch screen) with sm corners, a 16px icon 12px from its label, that fill surface-2 when active.
 *
 * Native menus (ConversationContextMenu and the like) draw these strings
 * directly. The Base UI menus and popovers (components/ui/dropdown-menu.tsx,
 * popover.tsx) take their panel, motion and rows from `MENU_POPUP` and
 * `menu-rows.tsx`, which follow the same measures. Select repeats that motion
 * on its own popup classes.
 */
export const MENU_POPOVER_SURFACE = [
  "max-w-[calc(100vw-32px)] overflow-y-auto",
  "rounded-lg bg-surface p-1.5 text-ink shadow-float",
  "board-menu-motion",
].join(" ");

export const MENU_POPOVER_WIDTH = "w-max min-w-[200px] max-w-[260px]";

export const MENU_ITEMS_CONTAINER = "flex w-full flex-col gap-0.5 outline-none";

export const MENU_ITEM = [
  "flex w-full cursor-pointer items-center justify-start gap-3 rounded-sm px-3 py-1.5 min-h-9 max-narrow:pointer-coarse:min-h-11 text-left text-body-regular",
  "text-ink outline-none transition-colors duration-[var(--button-transition-ms)] [&>svg]:size-4 [&>svg]:shrink-0 [&>svg]:text-ink-2",
  // The keyboard's place: the focus ring, drawn inside the row. A row that
  // holds focus only because the pointer is over it (`data-focused` without
  // `data-focus-visible`) is not that place, though Chromium still matches
  // `:focus-visible` for it.
  "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus [&[data-focused]:not([data-focus-visible])]:outline-0",
].join(" ");

export const MENU_ITEM_ACTIVE = "bg-surface-2";

export const MENU_ITEM_INTERACTIVE =
  "hover:bg-surface-2 focus-visible:bg-surface-2";

/** A destructive row: its words and icon in the failed ink. */
export const MENU_ITEM_DANGER =
  "text-state-failed-ink [&>svg]:text-state-failed-ink";

/** Inset separator shared by native menus and Base UI menus. */
export const MENU_SEPARATOR = "mx-1.5 my-1 h-px shrink-0 bg-line";

/** A small grey heading over a group of rows. */
export const MENU_HEADING =
  "px-3 pt-2 pb-1 text-caption-1-semibold text-ink-2";

/**
 * The panel of a Base UI popup (dropdown-menu.tsx, popover.tsx): the menu
 * surface and the menu motion, as transitions on Base UI's own state
 * attributes. In over 140ms from its anchor, up 4px and from .97; out over
 * 100ms, fading only. A press on the anchor in between reverses from where it
 * is, and reduced motion draws no transition (Base UI then unmounts at once).
 */
export const MENU_POPUP = [
  "rounded-lg bg-surface text-ink shadow-float outline-none",
  "origin-(--transform-origin) transition-[opacity,translate,scale] duration-[140ms] ease-standard",
  "data-starting-style:-translate-y-1 data-starting-style:scale-97 data-starting-style:opacity-0",
  "data-ending-style:opacity-0 data-ending-style:duration-100 data-ending-style:ease-exit",
  "motion-reduce:transition-none",
].join(" ");
