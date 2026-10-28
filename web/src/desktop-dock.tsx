import { createContext, useCallback, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import type { AnimatedIconProps } from "@ui/boardui/icons/animated-icon";
import { IconButton } from "@ui/components/ui/icon-button";
import { MENU_ITEMS_CONTAINER } from "@ui/boardui/overlays/menu-styles";

// The computer window's dock: its buttons' shared panel and the button that
// raises it, copied from core packages/console/src/components/computer/
// DesktopAudioControls.tsx (DesktopDockPanels, useDockPanel, DockPanelButton).
// Keep it in step with core; the styles are in styles.css ("dock panel").
// Changes: no `border`, since the shadow already draws the 1px edge, and
// `shadow-float` without its top highlight (styles.css, "dock-panel"), which
// stacked on that edge into a doubled top border under the toolbar.

/** How long the pointer may be off the button and its panel before it goes. */
const HOVER_GRACE_MS = 200;
/** The menu surface's opening and closing (`board-menu-motion`). */
const PANEL_MOTION_MS = 150;
/** How long the panel takes to move and resize to another button's contents. */
const PANEL_MORPH_MS = 200;
/** As `MENU_POPOVER_WIDTH`, the gap over the button and the least margin kept to the window's edge. */
const PANEL_WIDTH = { min: 200, max: 260 };
const PANEL_OFFSET = 8;
const PANEL_EDGE = 16;

type DockPanelEntry = {
  trigger: RefObject<HTMLButtonElement | null>;
  enter: () => void;
  exit: () => void;
};
type DockPanelsValue = {
  /** Where each button's contents go, by button. */
  slots: Readonly<Record<string, HTMLDivElement>>;
  /** The buttons whose contents are on the panel: the one it shows, and one fading out. */
  shown: readonly string[];
  register: (id: string, entry: DockPanelEntry) => () => void;
  claim: (id: string) => void;
  release: (id: string) => void;
  /** Keeps the panel shut while another of the window's panels is open (the clipboard's). */
  hold: (held: boolean) => void;
};
const DockPanelsContext = createContext<DockPanelsValue | null>(null);

/**
 * The one panel the dock's buttons share. Moving from one button to the next
 * does not close one card and open another: the card slides over the new
 * button and takes the size of its contents while they cross-fade. It is
 * placed inside the computer window rather than at the end of the page,
 * because a window shown fullscreen shows only what is inside it.
 */
export function DesktopDockPanels({ children }: { children: ReactNode }) {
  const entries = useRef(new Map<string, DockPanelEntry>());
  const [ids, setIds] = useState<readonly string[]>([]);
  // The buttons whose panel is open, the latest last: it is the one shown.
  const [open, setOpen] = useState<readonly string[]>([]);
  // While held, no button's panel shows, so it never lies under another.
  const [held, hold] = useState(false);
  const active = held ? undefined : open.at(-1);
  // What the panel shows: the active button's contents, kept while it closes.
  const [current, setCurrent] = useState<string>();
  const [leaving, setLeaving] = useState<string>();
  const [phase, setPhase] = useState<
    "closed" | "entering" | "open" | "exiting"
  >("closed");
  const [slots, setSlots] = useState<Record<string, HTMLDivElement>>({});
  const [container, setContainer] = useState<HTMLElement>();
  const surface = useRef<HTMLDivElement>(null);
  const slotRefs = useRef(
    new Map<string, (node: HTMLDivElement | null) => void>(),
  );
  const slotRef = (id: string) => {
    let ref = slotRefs.current.get(id);
    if (!ref) {
      ref = (node) =>
        setSlots((known) => {
          if (node ? known[id] === node : !known[id]) return known;
          const next = { ...known };
          if (node) next[id] = node;
          else delete next[id];
          return next;
        });
      slotRefs.current.set(id, ref);
    }
    return ref;
  };
  const register = useCallback((id: string, entry: DockPanelEntry) => {
    entries.current.set(id, entry);
    setIds((known) => (known.includes(id) ? known : [...known, id]));
    setContainer(
      (known) =>
        known ??
        entry.trigger.current?.closest<HTMLElement>('[role="dialog"]') ??
        document.body,
    );
    return () => {
      entries.current.delete(id);
      setIds((known) => known.filter((item) => item !== id));
      setOpen((known) => known.filter((item) => item !== id));
    };
  }, []);
  const claim = useCallback(
    (id: string) =>
      setOpen((known) =>
        known.at(-1) === id
          ? known
          : [...known.filter((item) => item !== id), id],
      ),
    [],
  );
  const release = useCallback(
    (id: string) =>
      setOpen((known) =>
        known.includes(id) ? known.filter((item) => item !== id) : known,
      ),
    [],
  );

  // Opening, switching and closing. A switch keeps the card and fades the
  // last contents out as the new ones come in.
  const shownRef = useRef<string>(undefined);
  useEffect(() => {
    const previous = shownRef.current;
    if (active) {
      shownRef.current = active;
      setCurrent(active);
      if (previous && previous !== active && phase !== "closed") {
        setLeaving(previous);
        const done = setTimeout(() => setLeaving(undefined), PANEL_MORPH_MS);
        return () => clearTimeout(done);
      }
      return undefined;
    }
    return undefined;
    // `phase` is read, not followed: only a new active button switches.
  }, [active]);
  useEffect(() => {
    if (active && (phase === "closed" || phase === "exiting")) {
      setPhase("entering");
      return undefined;
    }
    if (active && phase === "entering") {
      const done = setTimeout(() => setPhase("open"), PANEL_MOTION_MS);
      return () => clearTimeout(done);
    }
    if (!active && (phase === "entering" || phase === "open")) {
      setPhase("exiting");
      return undefined;
    }
    if (!active && phase === "exiting") {
      const done = setTimeout(() => {
        setPhase("closed");
        setLeaving(undefined);
        shownRef.current = undefined;
      }, PANEL_MOTION_MS);
      return () => clearTimeout(done);
    }
    return undefined;
  }, [active, phase]);

  // Where the card goes and how big it is: the shown contents' own width,
  // held to the menu's, centred below the button and inside the window.
  // `animate` false puts it there at once (when it opens).
  const place = useCallback(
    (animate: boolean) => {
      const card = surface.current;
      const slot = current ? slots[current] : undefined;
      const trigger = current
        ? entries.current.get(current)?.trigger.current
        : undefined;
      const parent = card?.offsetParent;
      if (!card || !slot || !trigger || !(parent instanceof HTMLElement))
        return;
      const style = getComputedStyle(card);
      const chromeX =
        parseFloat(style.paddingLeft) +
        parseFloat(style.paddingRight) +
        parseFloat(style.borderLeftWidth) +
        parseFloat(style.borderRightWidth);
      const chromeY =
        parseFloat(style.paddingTop) +
        parseFloat(style.paddingBottom) +
        parseFloat(style.borderTopWidth) +
        parseFloat(style.borderBottomWidth);
      // Layout sizes: the opening scale would shrink measured ones.
      slot.style.width = "max-content";
      const natural = slot.offsetWidth + chromeX;
      const width = Math.min(
        Math.max(natural, PANEL_WIDTH.min),
        PANEL_WIDTH.max,
        parent.clientWidth - 2 * PANEL_EDGE,
      );
      slot.style.width = `${width - chromeX}px`;
      const height = slot.offsetHeight + chromeY;
      const box = parent.getBoundingClientRect();
      const button = trigger.getBoundingClientRect();
      const originX = box.left + parent.clientLeft - parent.scrollLeft;
      const originY = box.top + parent.clientTop - parent.scrollTop;
      const center = button.left + button.width / 2 - originX;
      const left = Math.max(
        parent.scrollLeft + PANEL_EDGE,
        Math.min(
          center - width / 2,
          parent.scrollLeft + parent.clientWidth - PANEL_EDGE - width,
        ),
      );
      const top = button.bottom - originY + PANEL_OFFSET;
      if (!animate) card.removeAttribute("data-morph");
      card.style.left = `${left}px`;
      card.style.top = `${top}px`;
      card.style.width = `${width}px`;
      card.style.height = `${height}px`;
      card.style.setProperty("--trigger-anchor-point", `${center - left}px 0%`);
      if (!animate) {
        void card.offsetWidth;
        card.setAttribute("data-morph", "");
      }
    },
    [current, slots],
  );
  useLayoutEffect(() => {
    if (phase === "closed") return;
    place(phase !== "entering");
  }, [phase, place]);
  // Contents that change size (devices arriving) and a window that does.
  useEffect(() => {
    const slot = current ? slots[current] : undefined;
    if (phase === "closed" || !slot?.firstElementChild) return undefined;
    const observer = new ResizeObserver(() => place(true));
    observer.observe(slot.firstElementChild);
    const resize = () => place(false);
    window.addEventListener("resize", resize);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", resize);
    };
  }, [current, slots, phase, place]);

  const shown = useMemo(
    () =>
      phase === "closed"
        ? []
        : [leaving, current].filter(
            (id, index, all): id is string => !!id && all.indexOf(id) === index,
          ),
    [phase, leaving, current],
  );
  const value = useMemo(
    () => ({ slots, shown, register, claim, release, hold }),
    [slots, shown, register, claim, release],
  );
  const holder = useRef<DockPanelEntry>(undefined);
  holder.current = current ? entries.current.get(current) : undefined;
  // Listened to on the card itself: React's pointer enter and leave follow
  // the component tree, where each button's contents are not inside the card,
  // so moving onto them left it and the panel went as the pointer arrived.
  useEffect(() => {
    const card = surface.current;
    if (!card) return undefined;
    const enter = () => holder.current?.enter();
    const exit = () => holder.current?.exit();
    card.addEventListener("pointerenter", enter);
    card.addEventListener("pointerleave", exit);
    return () => {
      card.removeEventListener("pointerenter", enter);
      card.removeEventListener("pointerleave", exit);
    };
  }, [container]);
  return (
    <DockPanelsContext.Provider value={value}>
      {children}
      {container &&
        createPortal(
          // The menu surface (menu-styles), clipped rather than scrolled
          // while it changes size.
          <div
            ref={surface}
            hidden={phase === "closed"}
            data-morph=""
            data-entering={phase === "entering" || undefined}
            data-exiting={phase === "exiting" || undefined}
            className="dock-panel board-menu-motion absolute overflow-hidden rounded-lg bg-surface p-1.5"
          >
            {ids.map((id) => (
              <div
                key={id}
                ref={slotRef(id)}
                data-current={
                  (id === current && phase !== "closed") || undefined
                }
                inert={id !== current}
                className="dock-panel-slot absolute left-1.5 top-1.5"
              />
            ))}
          </div>,
          container,
        )}
    </DockPanelsContext.Provider>
  );
}

/** A dock button's place on the shared panel. */
function useDockPanel(
  trigger: RefObject<HTMLButtonElement | null>,
  open: boolean,
  enter: () => void,
  exit: () => void,
) {
  const panels = useContext(DockPanelsContext);
  if (!panels) throw Error("A dock panel button belongs in DesktopDockPanels");
  const { register, claim, release } = panels;
  const id = useId();
  const latest = useRef({ enter, exit });
  latest.current = { enter, exit };
  useEffect(
    () =>
      register(id, {
        trigger,
        enter: () => latest.current.enter(),
        exit: () => latest.current.exit(),
      }),
    [register, id, trigger],
  );
  useEffect(() => {
    if (open) claim(id);
    else release(id);
  }, [open, claim, release, id]);
  return {
    slot: panels.slots[id],
    shown: panels.shown.includes(id),
    claim: () => claim(id),
  };
}

/**
 * A dock button that does its one thing on a click and raises a panel over
 * itself while it is hovered or reached with the keyboard; the panel stays
 * while the pointer is on it and goes shortly after it leaves both. Tab on
 * the button moves into the panel (it sits at the end of the window), and
 * Escape closes the panel, not the window. `unavailable` keeps the click
 * from doing anything while the panel still opens, since a disabled button
 * takes no pointer at all. `onKey` sees the button's keys first and says
 * whether it used one.
 */
export function DockPanelButton({
  animate,
  label,
  pressed,
  onPress,
  unavailable = false,
  title,
  keyShortcuts,
  onKey,
  panelLabel,
  children,
}: {
  /** The button's icon, which moves as its state changes (ds 「图标动效」). */
  animate: Omit<AnimatedIconProps, "className" | "size">;
  label: string;
  pressed: boolean;
  onPress: () => void;
  unavailable?: boolean;
  title?: string | undefined;
  keyShortcuts?: string;
  onKey?: (event: React.KeyboardEvent) => boolean;
  panelLabel: string;
  children: ReactNode;
}) {
  const trigger = useRef<HTMLButtonElement>(null);
  const [hovered, setHovered] = useState(false);
  const [keyed, setKeyed] = useState(false);
  const open = hovered || keyed;
  const leave = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(leave.current), []);
  const exit = () => {
    clearTimeout(leave.current);
    leave.current = setTimeout(() => setHovered(false), HOVER_GRACE_MS);
  };
  const panel = useDockPanel(trigger, open, () => enter(), exit);
  // Coming back to a button whose panel is still going shows it again.
  function enter(event?: React.PointerEvent) {
    if (event?.pointerType === "touch") return;
    clearTimeout(leave.current);
    setHovered(true);
    panel.claim();
  }
  const key = () => {
    setKeyed(true);
    panel.claim();
  };
  const inside = (node: EventTarget | null) =>
    node instanceof Node &&
    (!!trigger.current?.contains(node) || !!panel.slot?.contains(node));
  return (
    <>
      <IconButton
        ref={trigger}
        variant="ghost"
        size="sm"
        className="size-8 max-narrow:size-11 aria-disabled:cursor-not-allowed aria-disabled:text-ink-3 aria-disabled:opacity-60"
        animate={animate}
        aria-pressed={pressed}
        aria-label={label}
        aria-disabled={unavailable || undefined}
        title={title}
        {...(keyShortcuts ? { "aria-keyshortcuts": keyShortcuts } : {})}
        onClick={() => {
          if (!unavailable) onPress();
        }}
        onPointerEnter={enter}
        onPointerLeave={exit}
        onFocus={(event) => {
          if (event.currentTarget.matches(":focus-visible")) key();
        }}
        onBlur={(event) => {
          if (!inside(event.relatedTarget)) setKeyed(false);
        }}
        onKeyDown={(event) => {
          if (onKey?.(event)) {
            event.preventDefault();
            key();
          } else if (event.key === "Tab" && !event.shiftKey && open) {
            const first =
              panel.slot?.querySelector<HTMLElement>("input, button");
            if (first) {
              event.preventDefault();
              key();
              first.focus();
            }
          } else if (event.key === "Escape" && open) {
            event.preventDefault();
            setKeyed(false);
            setHovered(false);
          }
        }}
      />
      {panel.slot &&
        panel.shown &&
        createPortal(
          // Focus and key events are the panel contents', not the card's.
          <div
            role="group"
            aria-label={panelLabel}
            className={MENU_ITEMS_CONTAINER}
            onBlur={(event) => {
              if (!inside(event.relatedTarget)) setKeyed(false);
            }}
            onKeyDown={(event) => {
              if (event.key !== "Escape") return;
              event.preventDefault();
              event.stopPropagation();
              setHovered(false);
              trigger.current?.focus();
              setKeyed(false);
            }}
          >
            {children}
          </div>,
          panel.slot,
        )}
    </>
  );
}


/** Shuts the dock's panel while `open` is true: for a panel of the window that opens by click. */
export function useDockHold(open: boolean) {
  const hold = useContext(DockPanelsContext)?.hold;
  useEffect(() => {
    hold?.(open);
    return () => hold?.(false);
  }, [hold, open]);
}
