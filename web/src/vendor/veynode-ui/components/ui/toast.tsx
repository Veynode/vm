import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ComponentProps,
  type ReactNode,
} from "react";
import { AnimatedIcon } from "../../boardui/icons/animated-icon";
import { X } from "../../icons";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui-style toast (the same slots as sonner's, minus its queue: the
 * app's ToastProvider owns scheduling, stacking and announcing; see
 * packages/ui/shadcn-sources.json), drawn in Veynode Soft.
 *
 * `ToastCard` is the toast itself: an ink pill (44px, `float` shadow), the
 * state as a small mark (none for plain information), one sentence (a long
 * one wraps with its lines kept together), then at most one text action after
 * a hairline divider. A timed toast has no close button: it leaves by itself
 * after `duration` ms, waits while hovered or focused, and Escape sends it
 * away once it has focus. Only a toast without a duration draws a quiet
 * close, so it can always be dismissed. No countdown bar. It pops up 16px
 * from the bottom over 200ms and sinks 8px as it leaves (120ms, the `exit`
 * token); reduced motion dismisses at once. Only the sentence is announced
 * (`alert` for an error, `status` otherwise), not the action's name.
 *
 * `Toast` hosts a card at the bottom centre in the native top layer, so it
 * stays usable above the active dialog. It stands above the home indicator
 * (`safe-area-inset-bottom`) and never covers a row of actions (see
 * `useClearOfActionRows`). One at a time: a new one replaces the old (`key`
 * it by its id).
 */
export type ToastVariant =
  | "information"
  | "success"
  | "error"
  | "waiting"
  | "working";

/** The one text action a toast may carry: a verb of up to four characters. */
export interface ToastAction {
  label: string;
  onAction: () => void;
}

/**
 * The toast's marks (ds `vs-toast-mark`): a tick or a "!" cut out of the
 * state's colour. They are marks, not icons: the design draws them smaller
 * than lucide's check and with a 2.75 stroke so they hold at 16px on the disc.
 */
function MarkGlyph({
  className,
  paths,
}: {
  className?: string | undefined;
  paths: readonly string[];
}) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2.75}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
      className={className}
    >
      {paths.map((d) => (
        <path key={d} d={d} />
      ))}
    </svg>
  );
}
function Exclamation({ className }: { className?: string }) {
  return (
    <MarkGlyph className={className} paths={["M12 7.5v5", "M12 16.5h.01"]} />
  );
}
function Tick({ className }: { className?: string }) {
  // `lucide-check` keeps the hook a done toast has always had.
  return (
    <MarkGlyph
      className={cn(className, "lucide-check")}
      paths={["M7.5 12.5l3 3 6-6.5"]}
    />
  );
}

// The mark: the state colour pulled toward on-action so it holds contrast on
// the inverted pill in both themes; the glyph is cut out in the pill's own
// colour. Plain information and `working` (a spinner) draw no disc.
const marks = {
  information: { mark: undefined, glyph: undefined },
  success: {
    mark: "bg-toast-mark-done",
    glyph: Tick,
  },
  error: {
    mark: "bg-toast-mark-failed",
    glyph: Exclamation,
  },
  waiting: {
    mark: "bg-toast-mark-waiting",
    glyph: Exclamation,
  },
  working: { mark: undefined, glyph: undefined },
} as const;

function ToastCard({
  title,
  variant = "information",
  closeLabel = "Dismiss notification",
  onDismiss,
  duration,
  action,
  trailing,
  className,
  ...props
}: Omit<ComponentProps<"div">, "title"> & {
  title: ReactNode;
  variant?: ToastVariant;
  /** Names the close button of a toast that has no duration. */
  closeLabel?: string;
  /** Called once the card has left (after its exit motion). */
  onDismiss?: () => void;
  /** Leaves by itself after this many ms. */
  duration?: number;
  /** The toast's text action; pressing it also dismisses the toast. */
  action?: ToastAction | undefined;
  /**
   * A control after the hairline, drawn by the caller (the web app's 详情
   * DetailTip holding the raw reason); styled like the action.
   */
  trailing?: ReactNode;
}) {
  const card = useRef<HTMLDivElement>(null);
  const dismiss = useRef(onDismiss);
  dismiss.current = onDismiss;
  const [exiting, setExiting] = useState(false);

  useEffect(() => {
    const node = card.current;
    if (!node || !onDismiss) return;
    const reduced = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    if (reduced) {
      if (exiting) dismiss.current?.();
      return;
    }
    const animation = node.animate(
      exiting
        ? [
            { opacity: 1, transform: "none" },
            { opacity: 0, transform: "translateY(8px) scale(.98)" },
          ]
        : [
            { opacity: 0, transform: "translateY(16px) scale(.92)" },
            { opacity: 1, transform: "none" },
          ],
      {
        duration: exiting ? 120 : 200,
        easing: exiting
          ? "cubic-bezier(0.4, 0, 1, 1)"
          : "cubic-bezier(0.3, 1.4, 0.5, 1)",
        fill: "both",
      },
    );
    if (exiting) animation.onfinish = () => dismiss.current?.();
    return () => {
      animation.onfinish = null;
      animation.cancel();
    };
  }, [exiting, Boolean(onDismiss)]);

  useEffect(() => {
    const node = card.current;
    if (!node || !duration || !onDismiss || exiting) return;
    let remaining = duration;
    let started = performance.now();
    let timer: ReturnType<typeof setTimeout>;
    let hovering = false;
    let focused = false;
    const pause = () => {
      clearTimeout(timer);
      remaining -= performance.now() - started;
    };
    const resume = () => {
      started = performance.now();
      timer = setTimeout(() => setExiting(true), Math.max(0, remaining));
    };
    const enter = () => {
      if (!focused) pause();
      hovering = true;
    };
    const leave = () => {
      hovering = false;
      if (!focused) resume();
    };
    const focus = () => {
      if (!focused && !hovering) pause();
      focused = true;
    };
    const blur = (event: FocusEvent) => {
      if (
        event.relatedTarget instanceof Node &&
        node.contains(event.relatedTarget)
      )
        return;
      focused = false;
      if (!hovering) resume();
    };
    resume();
    node.addEventListener("pointerenter", enter);
    node.addEventListener("pointerleave", leave);
    node.addEventListener("focusin", focus);
    node.addEventListener("focusout", blur);
    return () => {
      clearTimeout(timer);
      node.removeEventListener("pointerenter", enter);
      node.removeEventListener("pointerleave", leave);
      node.removeEventListener("focusin", focus);
      node.removeEventListener("focusout", blur);
    };
  }, [duration, Boolean(onDismiss), exiting]);

  const { glyph: Glyph, mark } = marks[variant];
  // A timed toast leaves by itself; one that stays needs a way to go.
  const closable = !!onDismiss && !duration;
  return (
    <div
      ref={card}
      data-slot="toast"
      data-notification={variant}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || !onDismiss || event.defaultPrevented)
          return;
        // Keys from a card the toast opened (a 详情 tip, portaled out of
        // it) reach here through React; that Escape closes the card only.
        if (
          !(event.target instanceof Node) ||
          !card.current?.contains(event.target)
        )
          return;
        event.preventDefault();
        event.stopPropagation();
        setExiting(true);
      }}
      className={cn(
        // One line draws as a pill (the radius is past half its height); a
        // sentence that wraps keeps the same corners instead of a capsule.
        "group/toast flex min-h-11 w-fit max-w-full items-center gap-2 rounded-xl bg-action py-2 pl-5 pr-5 text-on-action shadow-float",
        (Glyph || variant === "working") && "pl-3",
        (action || closable || trailing) && "pr-1.5",
        className,
      )}
      {...props}
    >
      {variant === "working" ? (
        <AnimatedIcon
          name="loader-circle"
          working
          className="size-4 shrink-0 text-toast-mark-working"
        />
      ) : (
        Glyph && (
          <span
            aria-hidden
            className={cn(
              "grid size-5 shrink-0 place-items-center rounded-full text-action",
              mark,
            )}
          >
            <Glyph className="size-4" />
          </span>
        )
      )}
      {/* Only the sentence is announced, not the action's name. */}
      {/* The base paragraph rule spreads lines for reading (it sets
          `--tw-leading`); a toast that wraps keeps its lines together. */}
      <p
        role={variant === "error" ? "alert" : "status"}
        aria-atomic="true"
        className="min-w-0 flex-1 break-words text-pretty text-body-medium [--tw-leading:var(--text-body-medium--line-height)] [overflow-wrap:anywhere]"
      >
        {title}
      </p>
      {(action || closable || trailing) && (
        <div className="flex shrink-0 items-center gap-1.5">
          {/* The hairline after the sentence, as tall as its line. */}
          <span aria-hidden className="h-5 w-px bg-on-action/20" />
          {action && (
            <button
              type="button"
              data-toast-action=""
              onClick={() => {
                action.onAction();
                if (onDismiss) setExiting(true);
              }}
              className="h-7 whitespace-nowrap rounded-full px-3 text-body-semibold text-on-action outline-none transition-[background-color,scale] duration-[var(--motion-fast)] hover:bg-on-action/15 active:scale-[.96] active:bg-on-action/20 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-on-action"
            >
              {action.label}
            </button>
          )}
          {trailing}
          {/* One that stays keeps a way to go that is not its action:
              pressing Try again is not how to say "not now". */}
          {closable && (
            <button
              type="button"
              aria-label={closeLabel}
              onClick={() => setExiting(true)}
              className="grid size-7 place-items-center rounded-full text-on-action/70 outline-none transition-colors duration-[var(--button-transition-ms)] hover:bg-on-action/15 hover:text-on-action focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-on-action"
            >
              <X className="size-4" aria-hidden />
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function Toast({
  message,
  onDismiss,
  dismissLabel,
  duration = 3500,
  variant = "success",
  action,
  trailing,
  className,
}: {
  message: string;
  onDismiss: () => void;
  /** Names the close button of a toast that has no duration. */
  dismissLabel?: string;
  /** `null` keeps it until it is dismissed. */
  duration?: number | null;
  variant?: ToastVariant;
  action?: ToastAction | undefined;
  /** A control of the caller's after the hairline (e.g. a 详情 tip). */
  trailing?: ReactNode;
  /** Hooks for tests or a feature (`chat-error`); never layout. */
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // Shown before paint, so it is placed (below) before it is ever seen.
  useLayoutEffect(() => {
    const node = ref.current;
    node?.showPopover();
    return () => node?.hidePopover();
  }, []);
  useClearOfActionRows(ref);
  return (
    <div
      ref={ref}
      popover="manual"
      data-slot="toast-host"
      data-toast={variant}
      className={cn(
        "fixed inset-auto bottom-[calc(24px+env(safe-area-inset-bottom,0px))] left-1/2 m-0 flex w-max max-w-[min(420px,calc(100vw-24px))] -translate-x-1/2 justify-center overflow-visible border-0 bg-transparent p-0",
        className,
      )}
    >
      <ToastCard
        title={message}
        variant={variant}
        {...(duration ? { duration } : {})}
        {...(dismissLabel ? { closeLabel: dismissLabel } : {})}
        {...(action ? { action } : {})}
        {...(trailing ? { trailing } : {})}
        onDismiss={onDismiss}
      />
    </div>
  );
}

/** The toast's resting distance from the bottom of the window, and its gap above a row it clears. */
const BOTTOM = 24;
const GAP = 12;
/**
 * A dialog's or a page's pinned actions (`DialogFooter`), and the state said
 * beside them; in a conversation the composer, whose send control a toast
 * must leave pressable.
 */
const ACTION_ROWS = ".modal-footer, .composer-frame";

/**
 * Keeps the toast off every row of actions on screen. At the bottom centre it
 * lands on a dialog's footer whenever the dialog reaches the bottom of the
 * window (a phone, a short window, a tall form), or on the composer of a
 * conversation, hiding the buttons and the failed badge the person is meant
 * to act on. Instead it rises to sit just
 * above the highest such row it would overlap, and comes back down when that
 * row goes. It follows rows that appear, move or leave while it shows: a
 * dialog opening under it, the window resizing, a dialog's entrance motion
 * settling.
 */
function useClearOfActionRows(ref: { readonly current: HTMLElement | null }) {
  useLayoutEffect(() => {
    const node = ref.current;
    if (!node) return;
    let frame = 0;
    let disposed = false;
    // The home indicator's height, which the resting place stands above.
    const probe = document.createElement("div");
    probe.style.cssText =
      "position:absolute;visibility:hidden;pointer-events:none;padding-bottom:env(safe-area-inset-bottom,0px)";
    node.append(probe);
    const resting = () =>
      BOTTOM + (parseFloat(getComputedStyle(probe).paddingBottom) || 0);
    const waiting = new WeakSet<Animation>();
    const place = () => {
      frame = 0;
      if (disposed) return;
      const view = window.innerHeight;
      const own = node.getBoundingClientRect();
      const rows = [...document.querySelectorAll<HTMLElement>(ACTION_ROWS)]
        .map((row) => ({ row, box: row.getBoundingClientRect() }))
        .filter(
          ({ box }) =>
            box.width > 0 &&
            box.height > 0 &&
            box.left < own.right &&
            box.right > own.left,
        );
      const rest = resting();
      let bottom = rest;
      // Rising above one row can land on another (a nested dialog's): go on
      // until the toast overlaps none.
      for (
        let moved = true, rounds = 0;
        moved && rounds < rows.length;
        rounds++
      ) {
        moved = false;
        const top = view - bottom - own.height;
        for (const { box } of rows) {
          if (box.bottom <= top - GAP || box.top >= view - bottom + GAP)
            continue;
          bottom = view - box.top + GAP;
          moved = true;
        }
      }
      // Never off the top of the window.
      bottom = Math.min(bottom, Math.max(rest, view - own.height - BOTTOM));
      const next = bottom === rest ? "" : `${bottom}px`;
      if (node.style.bottom !== next) node.style.bottom = next;
      if (!rows.length) return;
      // A dialog still growing into place moves its footer: place again once
      // that motion ends.
      for (const animation of document.getAnimations()) {
        const target =
          animation.effect instanceof KeyframeEffect
            ? animation.effect.target
            : null;
        if (
          !target ||
          waiting.has(animation) ||
          animation.playState !== "running" ||
          !rows.some(({ row }) => target.contains(row))
        )
          continue;
        waiting.add(animation);
        animation.finished.then(schedule, schedule);
      }
    };
    const schedule = () => {
      if (!frame && !disposed) frame = requestAnimationFrame(place);
    };
    place();
    // Changes inside the toast (its own placement, its tip) move no row.
    const observer = new MutationObserver((records) => {
      if (records.some((record) => !node.contains(record.target))) schedule();
    });
    observer.observe(document.body, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["class", "style", "hidden", "open"],
    });
    window.addEventListener("resize", schedule);
    return () => {
      disposed = true;
      probe.remove();
      cancelAnimationFrame(frame);
      observer.disconnect();
      window.removeEventListener("resize", schedule);
    };
  }, [ref]);
}

export { Toast, ToastCard };
