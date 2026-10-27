import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
/** Retains conditional inline menus (mentions/commands) through BoardUI exit motion. */
export function MenuPresence({ children }: { children: ReactNode }) {
  const visible =
    children !== null && children !== undefined && children !== false;
  const [retained, setRetained] = useState(children);
  if (visible && retained !== children) setRetained(children);
  const root = useRef<HTMLDivElement>(null);
  const interrupted = useRef<{ opacity: string; transform: string } | null>(null);
  useLayoutEffect(() => {
    const node = root.current?.firstElementChild as HTMLElement | null;
    if (!node) return;
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) {
      interrupted.current = null;
      if (!visible) setRetained(null);
      return;
    }
    // Veynode Soft: an inline menu (above the composer) grows from below in
    // 140ms (down 4px, from .97) and leaves in 100ms, fading only.
    const collapsed = { opacity: 0, transform: "translateY(4px) scale(.97)" };
    const expanded = { opacity: 1, transform: "none" };
    node.style.transformOrigin = "bottom left";
    const start = interrupted.current ?? (visible ? collapsed : expanded);
    interrupted.current = null;
    const animation = node.animate(
      [start, visible ? expanded : { ...start, opacity: 0 }],
      visible
        ? { duration: 140, easing: "cubic-bezier(0.2, 0, 0, 1)", fill: "both" }
        : { duration: 100, easing: "cubic-bezier(0.4, 0, 1, 1)", fill: "both" },
    );
    let active = true;
    void animation.finished.then(
      () => {
        if (active) {
          // Preserve the invisible final frame until React removes the retained menu.
          if (visible) animation.cancel();
          else setRetained(null);
        }
      },
      () => {},
    );
    return () => {
      active = false;
      if (animation.playState === "running" || animation.playState === "paused") {
        const style = getComputedStyle(node);
        interrupted.current = { opacity: style.opacity, transform: style.transform };
      }
      animation.cancel();
    };
  }, [visible]);
  return (
    <div
      ref={root}
      className="contents"
      inert={!visible}
      aria-hidden={!visible || undefined}
    >
      {visible ? children : retained}
    </div>
  );
}
