import { useEffect, useLayoutEffect, useRef, type RefObject } from "react";
import { flushSync } from "react-dom";

/**
 * A phone's swipe back, after Framework7's iOS theme: a touch that starts at
 * the screen's left edge carries the page in front with it, the list it was
 * opened from sliding in from a quarter of the way behind; let go past half
 * way, or with a flick to the right, and the page goes on out and the list
 * stays; otherwise it springs back.
 */
export const SWIPE_BACK = {
  /** How far from the left edge a touch may start, in px. */
  edge: 30,
  /** How far it moves before it is a swipe or a scroll, in px. */
  slop: 10,
  /** A release faster than this, in px/ms, goes back whatever the distance. */
  flick: 0.3,
  /** The longest the page takes to finish its way out or back, in ms. */
  finish: 300,
  /** The shortest, so a page let go near its end does not blink. */
  least: 120,
  /** How far behind the list starts, of the width. */
  parallax: 0.25,
  /** How much of the scrim veils the list under a page that has not moved. */
  veil: 0.5,
} as const;

/** Whether a page let go `dx` px to the right at `velocity` px/ms goes back. */
export function swipeOutcome(dx: number, velocity: number, width: number) {
  if (width <= 0 || dx <= 0) return "stay" as const;
  if (velocity > SWIPE_BACK.flick) return "back" as const;
  if (velocity < -SWIPE_BACK.flick) return "stay" as const;
  return dx / width > 0.5 ? ("back" as const) : ("stay" as const);
}

const BLOCKERS =
  '[role="dialog"][aria-modal="true"], dialog[open], [role="menu"], [role="listbox"]';

/**
 * An iPhone or iPad, whose own edge swipe goes back already: in a browser
 * tab, and in a home-screen app too, where the system carries the whole page
 * off over a picture of the one before (both at once showed the list twice).
 */
function browserOwnsEdge() {
  return (
    /iP(hone|ad|od)/.test(navigator.userAgent) ||
    (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1)
  );
}

/** Something open over the page, which takes the touch instead. */
function covered(root: HTMLElement) {
  return [...document.querySelectorAll<HTMLElement>(BLOCKERS)].some(
    (node) => !node.contains(root) && node.getClientRects().length > 0,
  );
}

/**
 * Swipe back from `front` to the list beside it (`under`, a selector among
 * `front`'s siblings) while `enabled`; `back` is what the page's own Back
 * does. Below the narrow breakpoint only, on touch, and not on iOS, in a
 * browser tab or a home-screen app (the system's own edge swipe goes back
 * through the same history entries; the two would fight). While the finger
 * is down the parent carries `data-swipe-back`, `under` `data-swipe-under`
 * and `front` `data-swipe-front`, which show the list behind (base.css); the
 * motion is transforms and the veil's opacity only.
 */
export function useSwipeBack(
  front: RefObject<HTMLElement | null>,
  under: string,
  enabled: boolean,
  back: () => void,
) {
  const backRef = useRef(back);
  backRef.current = back;
  useEffect(() => {
    const page = front.current;
    const root = page?.parentElement;
    if (!enabled || !page || !root) return;
    const reduced = () =>
      window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    let state: "idle" | "pending" | "dragging" | "settling" | "gone" = "idle";
    let gone: ReturnType<typeof setTimeout> | undefined;
    let list: HTMLElement | null = null;
    let startX = 0;
    let startY = 0;
    let width = 0;
    let dx = 0;
    let frame = 0;
    let samples: { x: number; t: number }[] = [];
    let motions: Animation[] = [];

    const paint = () => {
      frame = 0;
      const p = Math.min(1, dx / width);
      page.style.transform = `translate3d(${dx}px, 0, 0)`;
      if (list) {
        list.style.transform = reduced()
          ? ""
          : `translate3d(${-SWIPE_BACK.parallax * (1 - p) * width}px, 0, 0)`;
        list.style.setProperty(
          "--swipe-veil",
          String(SWIPE_BACK.veil * (1 - p)),
        );
      }
    };
    const clear = () => {
      clearTimeout(gone);
      if (frame) cancelAnimationFrame(frame);
      frame = 0;
      motions.forEach((motion) => motion.cancel());
      motions = [];
      root.removeAttribute("data-swipe-back");
      page.removeAttribute("data-swipe-front");
      page.style.transform = "";
      if (list) {
        list.removeAttribute("data-swipe-under");
        list.style.transform = "";
        list.style.removeProperty("--swipe-veil");
      }
      list = null;
      state = "idle";
    };
    const begin = () => {
      list = root.querySelector<HTMLElement>(`:scope > ${under}`);
      width = page.getBoundingClientRect().width;
      if (!list || width <= 0) {
        list = null;
        state = "idle";
        return false;
      }
      state = "dragging";
      root.setAttribute("data-swipe-back", "");
      page.setAttribute("data-swipe-front", "");
      list.setAttribute("data-swipe-under", "");
      return true;
    };
    const settle = (goBack: boolean) => {
      state = "settling";
      const target = goBack ? width : 0;
      const remaining = Math.abs(target - dx) / width;
      const finish = () => {
        if (!goBack) {
          clear();
          return;
        }
        // The list may come by a navigation, drawn a moment later: the page
        // stays where it went and the list where it came to until this
        // effect is ended by it (`enabled` false), or half a second passes.
        flushSync(() => backRef.current());
        state = "gone";
        gone = setTimeout(clear, 500);
      };
      if (reduced() || remaining === 0) {
        finish();
        return;
      }
      const duration = Math.max(
        SWIPE_BACK.least,
        Math.round(SWIPE_BACK.finish * remaining),
      );
      const timing = {
        duration,
        easing: "cubic-bezier(0.2, 0, 0, 1)",
        fill: "forwards" as const,
      };
      const p = Math.min(1, dx / width);
      motions = [
        page.animate(
          [
            { transform: `translate3d(${dx}px, 0, 0)` },
            { transform: `translate3d(${target}px, 0, 0)` },
          ],
          timing,
        ),
      ];
      if (list) {
        const from = -SWIPE_BACK.parallax * (1 - p) * width;
        const to = goBack ? 0 : -SWIPE_BACK.parallax * width;
        motions.push(
          list.animate(
            [
              {
                transform: `translate3d(${from}px, 0, 0)`,
                "--swipe-veil": String(SWIPE_BACK.veil * (1 - p)),
              },
              {
                transform: `translate3d(${to}px, 0, 0)`,
                "--swipe-veil": String(goBack ? 0 : SWIPE_BACK.veil),
              },
            ],
            timing,
          ),
        );
      }
      const mine = motions;
      void Promise.all(mine.map((motion) => motion.finished))
        .then(() => {
          if (motions === mine) finish();
        })
        .catch(() => {});
    };

    const onStart = (event: TouchEvent) => {
      if (state !== "idle" || event.touches.length !== 1) return;
      const touch = event.touches[0];
      if (
        !touch ||
        touch.clientX > SWIPE_BACK.edge ||
        !window.matchMedia("(width < 37.5rem)").matches ||
        browserOwnsEdge() ||
        covered(root) ||
        (event.target instanceof Element &&
          event.target.closest("[data-no-swipe-back]"))
      )
        return;
      state = "pending";
      startX = touch.clientX;
      startY = touch.clientY;
      dx = 0;
      samples = [{ x: touch.clientX, t: event.timeStamp }];
    };
    const onMove = (event: TouchEvent) => {
      if (state !== "pending" && state !== "dragging") return;
      const touch = event.touches[0];
      if (!touch) return;
      const moveX = touch.clientX - startX;
      const moveY = touch.clientY - startY;
      if (state === "pending") {
        if (
          Math.abs(moveX) < SWIPE_BACK.slop &&
          Math.abs(moveY) < SWIPE_BACK.slop
        )
          return;
        if (moveX <= Math.abs(moveY) || !event.cancelable) {
          state = "idle";
          return;
        }
        if (!begin()) return;
        // Placed before the list is first drawn, so it never shows where
        // it stands at rest.
        dx = Math.max(0, moveX);
        paint();
      }
      event.preventDefault();
      dx = Math.max(0, moveX);
      samples.push({ x: touch.clientX, t: event.timeStamp });
      while (samples.length > 2 && event.timeStamp - samples[0]!.t > 100)
        samples.shift();
      if (!frame) frame = requestAnimationFrame(paint);
    };
    const onEnd = (event: TouchEvent) => {
      if (state === "pending") {
        state = "idle";
        return;
      }
      if (state !== "dragging") return;
      if (frame) {
        cancelAnimationFrame(frame);
        paint();
      }
      const first = samples[0];
      const last = samples.at(-1);
      const elapsed = first && last ? last.t - first.t : 0;
      const velocity =
        first && last && elapsed > 0 ? (last.x - first.x) / elapsed : 0;
      settle(
        event.type === "touchend" &&
          swipeOutcome(dx, velocity, width) === "back",
      );
    };

    page.addEventListener("touchstart", onStart, { passive: true });
    page.addEventListener("touchmove", onMove, { passive: false });
    page.addEventListener("touchend", onEnd);
    page.addEventListener("touchcancel", onEnd);
    return () => {
      page.removeEventListener("touchstart", onStart);
      page.removeEventListener("touchmove", onMove);
      page.removeEventListener("touchend", onEnd);
      page.removeEventListener("touchcancel", onEnd);
      clear();
    };
  }, [front, under, enabled]);
}

/**
 * `useSwipeBack` for a page drawn by code that holds no ref to it (the chat
 * pane, drawn by a layout function): placed as the page's child, it takes
 * its parent for the page.
 */
export function SwipeBack({
  under,
  enabled,
  back,
}: {
  under: string;
  enabled: boolean;
  back: () => void;
}) {
  const mark = useRef<HTMLSpanElement>(null);
  const front = useRef<HTMLElement | null>(null);
  useLayoutEffect(() => {
    front.current = mark.current?.parentElement ?? null;
  });
  useSwipeBack(front, under, enabled, back);
  return <span ref={mark} hidden />;
}
