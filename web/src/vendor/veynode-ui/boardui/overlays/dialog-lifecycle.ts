/** Shared timing and retained-content lifecycle for DOM and native renderers. */
/**
 * Veynode Soft: the panel pops in over 200ms and leaves in 120ms
 * (`--motion-exit`); the scrim fades in over 200ms. A phone's sheet slides
 * up in the same 200ms and down in 160ms: a whole sheet's travel in 120ms
 * reads as a blink.
 */
export const DIALOG_MOTION = {
  enter: 200,
  exit: 120,
  scrim: 200,
  sheetExit: 160,
} as const;
export type DialogPresenceState<T> = {
  content: T;
  phase: "open" | "exiting";
  generation: number;
} | null;
export function openDialog<T>(
  content: T,
  generation: number,
): DialogPresenceState<T> {
  return { content, phase: "open", generation };
}
export function closeDialog<T>(
  state: DialogPresenceState<T>,
  reducedMotion: boolean,
): DialogPresenceState<T> {
  return !state || reducedMotion ? null : { ...state, phase: "exiting" };
}
export function finishDialogExit<T>(
  state: DialogPresenceState<T>,
  generation: number,
): DialogPresenceState<T> {
  return state?.phase === "exiting" && state.generation === generation
    ? null
    : state;
}
