import "./dialog-motion.css";
import {
  createContext,
  useContext,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";

// Local application behavior, shared by BoardUI, React Aria and native shells.
import { DIALOG_MOTION } from "./dialog-lifecycle";
export { DIALOG_MOTION } from "./dialog-lifecycle";
const Presence = createContext<{
  exiting: boolean;
  animateEnter: boolean;
  complete: () => void;
  register: () => () => void;
} | null>(null);

/**
 * Must sit OUTSIDE the conditional, so parent-driven close retains live React
 * content. Several conditionals under one presence each close on their own:
 * a list of them is never empty, so as one child none would ever be seen to
 * close.
 */
export function DialogPresence({
  children,
  animateEnter = true,
}: {
  children: ReactNode;
  /** Skip only the first enter animation for surfaces restored during initialization. */
  animateEnter?: boolean;
}) {
  if (Array.isArray(children))
    return children.map((child: ReactNode, slot) => (
      <OnePresence key={slot} animateEnter={animateEnter}>
        {child}
      </OnePresence>
    ));
  return <OnePresence animateEnter={animateEnter}>{children}</OnePresence>;
}

function OnePresence({
  children,
  animateEnter,
}: {
  children: ReactNode;
  animateEnter: boolean;
}) {
  const parent = useContext(Presence);
  const participants = useRef(0);
  const visible =
    children !== null && children !== undefined && children !== false;
  const [retained, setRetained] = useState(children);
  if (visible && retained !== children) setRetained(children);
  const exiting = !visible || !!parent?.exiting;
  useLayoutEffect(() => {
    if (!visible && participants.current === 0) setRetained(null);
  }, [visible]);
  return (
    <Presence.Provider
      value={{
        exiting,
        animateEnter,
        complete: () => {
          if (parent?.exiting) parent.complete();
          else setRetained(null);
        },
        register: () => {
          participants.current++;
          const unregister = parent?.register();
          return () => {
            participants.current--;
            unregister?.();
          };
        },
      }}
    >
      {visible ? children : retained}
    </Presence.Provider>
  );
}

/**
 * Where a dialog grows out of and shrinks back into, e.g. the computer's
 * mini screen in the details pane. Read when the dialog opens and when it
 * closes; nothing (or something not on screen) means the plain motion.
 */
export type DialogOrigin = () => Element | null | undefined;

/**
 * The transform that puts the dialog's grow target (`[data-dialog-grow-target]`
 * inside it, else the whole panel) over the origin, scaling from the panel's
 * top left corner; null when there is no usable origin.
 */
function originTransform(panel: HTMLElement, origin: DialogOrigin | undefined) {
  const from = origin?.();
  if (!from || !from.isConnected) return null;
  const m = from.getBoundingClientRect();
  if (m.width < 8 || m.height < 8) return null;
  if (m.bottom < 0 || m.right < 0 || m.top > innerHeight || m.left > innerWidth)
    return null;
  const b = panel.getBoundingClientRect();
  const target =
    panel.querySelector<HTMLElement>("[data-dialog-grow-target]") ?? panel;
  const s = target.getBoundingClientRect();
  if (!b.width || !s.width) return null;
  const k = m.width / s.width;
  const x = m.left - b.left - (s.left - b.left) * k;
  const y = m.top - b.top - (s.top - b.top) * k;
  return `translate(${x}px, ${y}px) scale(${k})`;
}

export function animateDialog(
  panel: HTMLElement,
  overlay: HTMLElement | null,
  exiting: boolean,
  origin?: DialogOrigin,
) {
  panel.dataset["dialogMotion"] = exiting ? "exit" : "enter";
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return [];
  // Out of an origin: 200ms on the standard curve from the origin's place
  // and size, the header fading in over its last 120ms; back into it in 200ms on the exit
  // curve (ds 打开电脑). The scrim does as it always does.
  // A sheet (`data-sheet`, below the narrow breakpoint) slides up from the
  // screen's foot and back down, the scrim fading as always.
  if (
    panel.hasAttribute("data-sheet") &&
    window.matchMedia("(width < 37.5rem)").matches
  ) {
    const current = getComputedStyle(panel);
    const motions = [
      panel.animate(
        exiting
          ? [
              { transform: current.transform },
              { transform: "translateY(100%)" },
            ]
          : [{ transform: "translateY(100%)" }, { transform: "none" }],
        {
          duration: exiting ? DIALOG_MOTION.sheetExit : DIALOG_MOTION.enter,
          easing: exiting
            ? "cubic-bezier(0.4, 0, 1, 1)"
            : "cubic-bezier(0.2, 0, 0, 1)",
          fill: "both",
        },
      ),
    ];
    if (overlay)
      motions.push(
        overlay.animate(
          exiting
            ? [{ opacity: getComputedStyle(overlay).opacity }, { opacity: 0 }]
            : [{ opacity: 0 }, { opacity: 1 }],
          {
            duration: exiting ? DIALOG_MOTION.sheetExit : DIALOG_MOTION.scrim,
            easing: exiting
              ? "cubic-bezier(0.4, 0, 1, 1)"
              : "cubic-bezier(0.2, 0, 0, 1)",
            fill: "both",
          },
        ),
      );
    return motions;
  }
  const grown = originTransform(panel, origin);
  if (grown) {
    // A hook for checks: this dialog moved out of (or back into) its origin.
    panel.dataset["dialogOrigin"] = exiting ? "shrink" : "grow";
    const motions = [
      panel.animate(
        exiting
          ? [
              { transform: "none", transformOrigin: "0 0", opacity: 1 },
              { offset: 0.7, opacity: 1 },
              { transform: grown, transformOrigin: "0 0", opacity: 0 },
            ]
          : [
              { transform: grown, transformOrigin: "0 0", opacity: 0.4 },
              { offset: 0.3, opacity: 1 },
              { transform: "none", transformOrigin: "0 0", opacity: 1 },
            ],
        {
          duration: 200,
          easing: exiting
            ? "cubic-bezier(0.4, 0, 1, 1)"
            : "cubic-bezier(0.2, 0, 0, 1)",
          fill: "both",
        },
      ),
    ];
    const head = panel.querySelector<HTMLElement>(".modal-head");
    if (head && !exiting)
      motions.push(
        head.animate([{ opacity: 0 }, { opacity: 1 }], {
          duration: 120,
          delay: 80,
          easing: "cubic-bezier(0.2, 0, 0, 1)",
          fill: "both",
        }),
      );
    if (overlay)
      motions.push(
        overlay.animate(
          exiting
            ? [{ opacity: getComputedStyle(overlay).opacity }, { opacity: 0 }]
            : [{ opacity: 0 }, { opacity: 1 }],
          {
            duration: exiting ? 200 : DIALOG_MOTION.scrim,
            easing: exiting
              ? "cubic-bezier(0.4, 0, 1, 1)"
              : "cubic-bezier(0.2, 0, 0, 1)",
            fill: "both",
          },
        ),
      );
    return motions;
  }
  // In: lifts 8px and grows from .96 on the pop curve. Out: shrinks to .98
  // on the exit curve, faster than it came.
  const options: KeyframeAnimationOptions = {
    duration: exiting ? DIALOG_MOTION.exit : DIALOG_MOTION.enter,
    easing: exiting
      ? "cubic-bezier(0.4, 0, 1, 1)"
      : "cubic-bezier(0.3, 1.4, 0.5, 1)",
    fill: "both",
  };
  const current = getComputedStyle(panel);
  const motions = [
    panel.animate(
      exiting
        ? [
            { opacity: current.opacity, transform: current.transform },
            { opacity: 0, transform: "scale(0.98)" },
          ]
        : [
            { opacity: 0, transform: "translateY(8px) scale(0.96)" },
            { opacity: 1, transform: "none" },
          ],
      options,
    ),
  ];
  if (overlay)
    motions.push(
      overlay.animate(
        exiting
          ? [{ opacity: getComputedStyle(overlay).opacity }, { opacity: 0 }]
          : [{ opacity: 0 }, { opacity: 1 }],
        {
          duration: exiting ? DIALOG_MOTION.exit : DIALOG_MOTION.scrim,
          easing: exiting
            ? "cubic-bezier(0.4, 0, 1, 1)"
            : "cubic-bezier(0.2, 0, 0, 1)",
          fill: "both",
        },
      ),
    );
  return motions;
}

/** The owning presence controls removal; cancellation never completes an obsolete exit. */
export function useDialogMotion(
  panel: RefObject<HTMLElement | null>,
  overlay?: RefObject<HTMLElement | null>,
  origin?: DialogOrigin,
) {
  const growFrom = useRef(origin);
  growFrom.current = origin;
  const presence = useContext(Presence);
  if (import.meta.env.DEV && !presence)
    throw new Error(
      "Dialog requires DialogPresence outside its conditional mount.",
    );
  useLayoutEffect(() => presence?.register(), []);
  const complete = useRef(presence?.complete);
  complete.current = presence?.complete;
  const exiting = presence?.exiting ?? false;
  const animateEnter = presence?.animateEnter ?? true;
  useLayoutEffect(() => {
    if (!panel.current) return;
    let active = true;
    const motions =
      exiting || animateEnter
        ? animateDialog(
            panel.current,
            overlay?.current ?? null,
            exiting,
            growFrom.current,
          )
        : [];
    void Promise.all(motions.map((m) => m.finished))
      .then(() => {
        if (!active) return;
        if (exiting) complete.current?.();
        else motions.forEach((m) => m.cancel());
      })
      .catch(() => {
        if (active && exiting) complete.current?.();
      });
    return () => {
      active = false;
      motions.forEach((m) => m.cancel());
    };
  }, [animateEnter, exiting, panel, overlay]);
}

/** Native top-layer dialogs retain their browser focus/scroll semantics until close completes. */
export function useNativeDialogMotion(
  dialog: RefObject<HTMLDialogElement | null>,
) {
  const motions = useRef<Animation[]>([]);
  const generation = useRef(0);
  const closing = useRef(false);
  const cancel = () => {
    generation.current++;
    motions.current.forEach((m) => m.cancel());
    motions.current = [];
  };
  useLayoutEffect(() => () => cancel(), []);
  const open = () => {
    cancel();
    closing.current = false;
    if (!dialog.current) return;
    dialog.current.showModal();
    motions.current = animateDialog(dialog.current, null, false);
    const token = generation.current;
    void Promise.all(motions.current.map((m) => m.finished))
      .then(() => {
        if (token === generation.current)
          motions.current.forEach((m) => m.cancel());
      })
      .catch(() => {});
  };
  const close = () => {
    if (!dialog.current?.open || closing.current) return;
    cancel();
    closing.current = true;
    const element = dialog.current;
    const token = generation.current;
    motions.current = animateDialog(element, null, true);
    const finish = () => {
      if (token !== generation.current) return;
      element.close();
      closing.current = false;
      motions.current.forEach((m) => m.cancel());
    };
    if (!motions.current.length) finish();
    else
      void Promise.all(motions.current.map((m) => m.finished))
        .then(finish)
        .catch(finish);
  };
  return { open, close };
}

/**
 * Only the top dialog handles keyboard dismissal; a native dialog owns the top
 * layer. A page that shares the dialog layer without being a dialog (Settings,
 * `data-dialog-layer`) counts too: a dialog it was opened over is not on top.
 */
export function isTopDialog(panel: HTMLElement | null) {
  if (!panel) return false;
  const native = Array.from(
    document.querySelectorAll<HTMLDialogElement>("dialog[open]"),
  ).at(-1);
  if (native) return native === panel || native.contains(panel);
  return (
    Array.from(
      // React Aria modal dialogs may omit aria-modal on the role=dialog node.
      // They still own keyboard dismissal while visible above a custom shell.
      document.querySelectorAll<HTMLElement>(
        '[role="dialog"], [data-dialog-layer]',
      ),
    )
      .filter(
        (element) =>
          element.getClientRects().length > 0 &&
          // A menu still fading out (React Aria marks its popover) is no
          // longer a layer: the dialog its item opened is on top already.
          !element.closest("[data-exiting]"),
      )
      .at(-1) === panel
  );
}
