import { flushSync } from "react-dom";
import { SWIPE_BACK } from "@veynode/ui/boardui/swipe-back";

/**
 * A phone's page motion, as Framework7's iOS theme moves pages: a page opened
 * from a list (a conversation, a Settings page) or from
 * another page comes in from the right over the one it leaves, which drifts
 * a quarter to the left under a veil; going back is the same, reversed. It is
 * the motion `useSwipeBack` lets a finger drive. The browser takes the screen
 * before and after as two pictures (View Transitions), but the chats' list
 * and chat move as themselves (`movePanes`); without them, with
 * reduced motion, on a wider screen, or while a finger is carrying the page
 * off already, the page just changes.
 */
type PageMotion = "push" | "pop";

/** Where a phone stands, by address: its part of the app and how deep in it. */
function pageDepth(pathname: string) {
  const parts = pathname.split("/").filter(Boolean);
  const family = parts[0] ?? "";
  if (family === "settings") return { family, depth: parts.length - 1 };
  // Space is one page: its places filter it, and none is under another.
  if (family === "space") return { family, depth: 0 };
  if (family === "c") return { family: "", depth: parts.length > 1 ? 1 : 0 };
  return { family: "", depth: 0 };
}

/** The motion from one address to another: deeper is a push, shallower a pop. */
export function pageMotion(from: string, to: string): PageMotion | undefined {
  const before = pageDepth(from);
  const after = pageDepth(to);
  if (before.family !== after.family || before.depth === after.depth)
    return undefined;
  return after.depth > before.depth ? "push" : "pop";
}

/** The longest the screen is held for the page coming in to be drawn. */
const DRAW_LIMIT_MS = 500;
let latest = 0;

function moves() {
  return (
    typeof document !== "undefined" &&
    typeof document.startViewTransition === "function" &&
    window.matchMedia("(width < 37.5rem)").matches &&
    !window.matchMedia("(prefers-reduced-motion: reduce)").matches &&
    // A finger carried the page off already (`useSwipeBack`), or a chat's
    // pane is crossfading (`swapPane`).
    !document.querySelector("[data-swipe-back]") &&
    !document.documentElement.classList.contains("pane-swap")
  );
}

/** The chats' list and chat pane, when the motion is between the two. */
function chatsPanes(motion: PageMotion) {
  if (document.querySelector(".settings-overlay")) return undefined;
  const app = document.querySelector<HTMLElement>(".chat-app");
  const front = app?.querySelector<HTMLElement>(":scope > .chat-main");
  const under = app?.querySelector<HTMLElement>(":scope > .chat-sidebar");
  if (!app || !front || !under) return undefined;
  // The page shown now: the list for a push, the chat for a pop.
  const shown = motion === "push" ? under : front;
  if (shown.getClientRects().length === 0) return undefined;
  return { app, front, under };
}

/**
 * The chats move as live panes, by the swipe back's own marks and styles
 * (`data-swipe-back`, base.css): two pictures of the screen end at the
 * page's foot in a browser tab, where the list runs on under the browser's
 * bottom bar (pwa.css), and its rows there would vanish for the motion.
 */
function movePanes(
  motion: PageMotion,
  panes: { app: HTMLElement; front: HTMLElement; under: HTMLElement },
  update: () => void | Promise<void>,
) {
  const { app, front, under } = panes;
  // The places capsule stands on the list (fixed at the screen's foot,
  // SidebarTabs) and goes with it, darkened as its veil darkens it.
  const foot = document.querySelector<HTMLElement>("[data-tabs-foot]");
  const root = document.documentElement;
  const width = app.getBoundingClientRect().width;
  const behind = -SWIPE_BACK.parallax * width;
  root.dataset["pageMotion"] = motion;
  app.setAttribute("data-swipe-back", "");
  front.setAttribute("data-swipe-front", "");
  under.setAttribute("data-swipe-under", "");
  const clear = () => {
    app.removeAttribute("data-swipe-back");
    front.removeAttribute("data-swipe-front");
    under.removeAttribute("data-swipe-under");
    front.style.transform = "";
    under.style.transform = "";
    under.style.removeProperty("--swipe-veil");
    if (foot) {
      foot.style.transform = "";
      foot.style.filter = "";
    }
    delete root.dataset["pageMotion"];
  };
  const timing = {
    duration: 350,
    easing: "cubic-bezier(0.2, 0, 0, 1)",
    fill: "forwards" as const,
  };
  const at = (x: number) => ({ transform: `translate3d(${x}px, 0, 0)` });
  const veiled = (x: number, veil: number) => ({
    ...at(x),
    "--swipe-veil": String(veil),
  });
  const place = (
    pane: HTMLElement,
    frame: { transform: string; "--swipe-veil"?: string },
  ) => {
    pane.style.transform = frame.transform;
    if (frame["--swipe-veil"] !== undefined)
      pane.style.setProperty("--swipe-veil", frame["--swipe-veil"]);
  };
  const [frontFrom, frontTo] =
    motion === "push" ? [at(width), at(0)] : [at(0), at(width)];
  const [underFrom, underTo] =
    motion === "push"
      ? [veiled(0, 0), veiled(behind, SWIPE_BACK.veil)]
      : [veiled(behind, SWIPE_BACK.veil), veiled(0, 0)];
  // Both panes stand where the motion starts before either is first drawn
  // (the chat coming in never shows at rest).
  const shade = (frame: { transform: string; "--swipe-veil"?: string }) => ({
    transform: frame.transform,
    filter: `brightness(${1 - 0.32 * Number(frame["--swipe-veil"] ?? 0)})`,
  });
  place(front, frontFrom);
  place(under, underFrom);
  if (foot) Object.assign(foot.style, shade(underFrom));
  if (motion === "push") flushSync(() => void update());
  // The pane shown anew is laid out over the next frames: the motion starts
  // after them, or a long first frame would eat its start.
  requestAnimationFrame(() =>
    requestAnimationFrame(() => {
      const motions = [
        front.animate([frontFrom, frontTo], timing),
        under.animate([underFrom, underTo], timing),
      ];
      if (foot)
        motions.push(foot.animate([shade(underFrom), shade(underTo)], timing));
      void Promise.all(motions.map((one) => one.finished))
        .catch(() => {})
        .finally(async () => {
          if (motion === "pop") {
            // The list may come by a navigation, drawn a moment later: the
            // panes hold where they went until it is, or half a second passes.
            const holder: { drawn?: Promise<void> } = {};
            flushSync(() => {
              const result = update();
              if (result instanceof Promise) holder.drawn = result;
            });
            if (holder.drawn)
              await Promise.race([
                holder.drawn,
                new Promise((done) => setTimeout(done, DRAW_LIMIT_MS)),
              ]);
          }
          clear();
          motions.forEach((one) => one.cancel());
        });
    }),
  );
}

/**
 * Makes `update` as a page motion. `update` changes state React draws at once
 * (it runs in `flushSync`), or returns a promise that settles once the page
 * coming in is drawn (a navigation; held at most half a second).
 */
export function pageTransition(
  motion: PageMotion | undefined,
  update: () => void | Promise<void>,
) {
  if (!motion || !moves()) {
    void update();
    return;
  }
  const panes = chatsPanes(motion);
  if (panes) {
    movePanes(motion, panes, update);
    return;
  }
  const root = document.documentElement;
  root.dataset["pageMotion"] = motion;
  const mine = ++latest;
  const transition = document.startViewTransition(async () => {
    const holder: { drawn?: Promise<void> } = {};
    flushSync(() => {
      const result = update();
      if (result instanceof Promise) holder.drawn = result;
    });
    const drawn = holder.drawn;
    if (drawn)
      await Promise.race([
        drawn,
        new Promise((done) => setTimeout(done, DRAW_LIMIT_MS)),
      ]);
  });
  void transition.updateCallbackDone.catch(() => {});
  void transition.finished
    .catch(() => {})
    .finally(() => {
      if (mine === latest) delete root.dataset["pageMotion"];
    });
}
