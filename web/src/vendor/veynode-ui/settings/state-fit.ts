import { useCallback } from "react";

/**
 * The least of a row's name that is still worth showing: about four letters
 * and the ellipsis. A state keeps its natural width until the name would be
 * cut below this; then it gives way to a dot.
 */
export const NAME_MINIMUM = 48;
/** What a row's meta must be left to be worth its place; under it, it goes. */
export const META_MINIMUM = 48;

/**
 * Whether a name of `natural` px that was given `room` px is still readable:
 * its whole length when that is shorter than the minimum, else the minimum.
 * Pure, so the rule is tested without a layout engine.
 */
export function nameFits(room: number, natural: number): boolean {
  return room >= Math.min(natural, NAME_MINIMUM);
}

/**
 * A name or meta that may be cut carries its words as its `title`, set here
 * (not in the markup) so the words are not written twice; a title the caller
 * gave it stays.
 */
function titled(part: HTMLElement | null) {
  if (!part) return;
  if (part.getAttribute("title") && !part.hasAttribute("data-row-titled"))
    return;
  const words = part.textContent?.trim();
  if (!words) return;
  part.setAttribute("title", words);
  part.setAttribute("data-row-titled", "");
}

/**
 * One pass over a row: draws its state whole and its meta, then takes back
 * what does not fit (see `useStateFit`). Exported for tests.
 */
export function fitRow(element: HTMLElement) {
  const status = element.querySelector<HTMLElement>("[data-row-status]");
  const name = element.querySelector<HTMLElement>("[data-row-name]");
  if (!name || !element.offsetWidth) return;
  const squeeze = element.querySelector<HTMLElement>("[data-row-squeeze]");
  titled(name);
  titled(element.querySelector<HTMLElement>("[data-row-meta]"));
  status?.removeAttribute("data-compact");
  squeeze?.removeAttribute("data-squeezed");
  // A meta shorter than the minimum is kept while it is shown whole.
  if (
    squeeze &&
    squeeze.clientWidth < Math.min(META_MINIMUM, squeeze.scrollWidth)
  )
    squeeze.setAttribute("data-squeezed", "");
  if (!status) return;
  if (nameFits(name.clientWidth, name.scrollWidth)) {
    status.removeAttribute("title");
    return;
  }
  status.setAttribute("data-compact", "");
  const words = status.textContent?.trim();
  if (words) status.setAttribute("title", words);
}

/** Fits the row now and whenever its size or its words change; returns the way to stop. */
export function watchRow(element: HTMLElement | null) {
  if (!element || typeof ResizeObserver === "undefined") return;
  const observer = new ResizeObserver(() => fitRow(element));
  const watch = () => {
    observer.disconnect();
    observer.observe(element);
    for (const part of element.querySelectorAll(
      "[data-row-status], [data-row-name]",
    ))
      observer.observe(part);
  };
  watch();
  // A state or a name that arrives or changes its words (a check that ends)
  // without the row changing size is measured again.
  const changes = new MutationObserver(() => {
    watch();
    fitRow(element);
  });
  changes.observe(element, {
    childList: true,
    subtree: true,
    characterData: true,
  });
  return () => {
    observer.disconnect();
    changes.disconnect();
  };
}

/**
 * A row's state stays visible: short of room the meta goes first (it is
 * cut to a sliver, and a sliver is hidden), then the name is cut (ellipsis)
 * and the state keeps its natural width. Only where even the name at its
 * minimum does not fit next to it does the state become a dot (its word kept
 * for assistive technology and as the dot's `title`).
 *
 * The row marks its parts: `data-row-name` is the element that is cut (the
 * one that truncates its own text), `data-row-status` holds the state and
 * `data-row-squeeze` is the end of the row that gives way first. The
 * state's badges draw their dot from the `data-compact` attribute set here
 * (`StateTag`), directly on the element so a re-render of the row never
 * undoes it. Each pass is a trial: draw the state whole, measure what the
 * name was left, and draw what fits, all before paint.
 *
 * Returns the row's `ref`: it runs with the commit, before paint.
 */ export function useStateFit() {
  return useCallback(watchRow, []);
}
