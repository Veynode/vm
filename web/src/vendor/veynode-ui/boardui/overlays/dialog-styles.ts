// Shared visual recipe from the BoardUI Settings Modal, in Veynode Soft: a
// surface with xl corners and the `modal` shadow over the scrim; the title in
// title-2; a 28px round close.
export const DIALOG_SURFACE = "rounded-xl bg-surface text-ink shadow-modal outline-none";
export const DIALOG_HEADER = "flex shrink-0 items-center justify-between gap-2 px-6 pt-5 pb-2 max-narrow:px-4 max-narrow:pt-4 [&_h2]:min-w-0 [&_h2]:truncate [&_h2]:text-title-2-semibold";
export const DIALOG_CONTENT = "px-6 pb-5 max-narrow:px-4";
export const DIALOG_CLOSE = "flex size-7 shrink-0 cursor-pointer items-center justify-center rounded-full bg-transparent text-ink-2 transition-colors duration-[var(--button-transition-ms)] hover:bg-surface-2 hover:text-ink outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus";
/** The scrim under a dialog: the whole window, painted by the Dialog's backdrop. */
export const DIALOG_BACKDROP = "fixed inset-0 bg-scrim";
/**
 * The frame the surface is centred in: the whole window, clear of the notch,
 * the home indicator and the Mac's traffic lights. It does not take presses
 * (they fall through to the scrim, which dismisses); the surface does.
 */
export const DIALOG_VIEWPORT =
  "pointer-events-none fixed inset-0 flex items-center justify-center pl-[calc(1rem+var(--safe-left,0px))] pr-[calc(1rem+var(--safe-right,0px))] pt-[calc(1rem+var(--window-controls-inset,0px)+var(--safe-top,0px))] pb-[calc(1rem+var(--safe-bottom,0px))]";
