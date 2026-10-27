import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type RefObject,
} from "react";
// Veynode Soft: a menu grows from its trigger in 140ms (up 4px, from .97) and
// leaves in 100ms, fading only.
const collapsed = { opacity: 0, transform: "translateY(-4px) scale(.97)" };
const expanded = { opacity: 1, transform: "none" };
const timing = { duration: 140, easing: "cubic-bezier(0.2, 0, 0, 1)", fill: "both" as FillMode };
const exitTiming = { duration: 100, easing: "cubic-bezier(0.4, 0, 1, 1)", fill: "both" as FillMode };
/** The menu motion; native menus stay in the top layer through exit. */
export function useNativeMenuMotion(
  panel: RefObject<HTMLElement | null>,
  trigger?: RefObject<HTMLElement | null>,
) {
  const [isOpen, setOpen] = useState(false);
  const animation = useRef<Animation | null>(null);
  const generation = useRef(0);
  const snapshot = (node: HTMLElement) => {
    const style = getComputedStyle(node);
    return { opacity: style.opacity, transform: style.transform };
  };
  const open = useCallback(() => {
    const node = panel.current;
    if (!node) return;
    if (node.matches(":popover-open") && !node.dataset["menuExiting"]) return;
    const from = animation.current ? snapshot(node) : collapsed;
    const id = ++generation.current;
    animation.current?.cancel();
    animation.current = null;
    node.inert = false;
    delete node.dataset["menuExiting"];
    if (!node.matches(":popover-open")) node.showPopover();
    setOpen(true);
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const motion = node.animate([from, expanded], timing);
    animation.current = motion;
    void motion.finished.then(
      () => {
        if (generation.current !== id) return;
        motion.cancel();
        animation.current = null;
      },
      () => {},
    );
  }, [panel]);
  const close = useCallback(() => {
    const node = panel.current;
    if (!node?.matches(":popover-open") || node.dataset["menuExiting"]) return;
    const from = animation.current ? snapshot(node) : expanded;
    animation.current?.cancel();
    animation.current = null;
    setOpen(false);
    const id = ++generation.current;
    const finish = () => {
      if (generation.current !== id) return;
      const restore =
        !!trigger?.current &&
        (node.contains(document.activeElement) ||
          document.activeElement === document.body);
      node.hidePopover();
      animation.current?.cancel();
      animation.current = null;
      node.inert = false;
      delete node.dataset["menuExiting"];
      setOpen(false);
      if (
        restore &&
        trigger?.current?.isConnected &&
        !document.querySelector('[role="dialog"][aria-modal="true"]')
      )
        trigger.current.focus({ preventScroll: true });
    };
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) {
      finish();
      return;
    }
    node.dataset["menuExiting"] = "true";
    node.inert = true;
    animation.current = node.animate([from, { ...from, opacity: 0 }], exitTiming);
    void animation.current.finished.then(finish, () => {});
  }, [panel, trigger]);
  const toggle = useCallback(() => {
    if (
      panel.current?.matches(":popover-open") &&
      !panel.current.dataset["menuExiting"]
    )
      close();
    else open();
  }, [panel, open, close]);
  useEffect(() => {
    if (!isOpen) return;
    const outside = (e: PointerEvent) => {
      if (
        e.button === 0 &&
        !panel.current?.contains(e.target as Node) &&
        !trigger?.current?.contains(e.target as Node)
      )
        close();
    };
    const escape = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close();
      }
    };
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", escape, true);
    return () => {
      document.removeEventListener("pointerdown", outside, true);
      document.removeEventListener("keydown", escape, true);
    };
  }, [isOpen, panel, trigger, close]);
  useEffect(
    () => () => {
      generation.current++;
      animation.current?.cancel();
    },
    [],
  );
  return { open, close, toggle };
}
