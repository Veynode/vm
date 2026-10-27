import type { HTMLAttributes, ReactNode } from "react";
import { AnimatedIcon } from "../boardui/icons/animated-icon";
import { cx } from "../boardui/utils/cx";

/**
 * Each status as one of the Soft states: done (finished, connected, ready),
 * working (under way), waiting (needs the person, or held), failed. Anything
 * else is neutral and says its own word.
 */
export type StateTone = "done" | "failed" | "waiting" | "working" | "neutral";
/** The badge: colour says the state (fine, needs the person, failed); a fact and work under way are grey. */
const badgeStyles: Record<StateTone, string> = {
  done: "bg-state-done-tint text-state-done-ink",
  working: "bg-ink/6 text-ink-2",
  neutral: "bg-ink/6 text-ink-2",
  failed: "bg-state-failed-tint text-state-failed-ink",
  waiting: "bg-state-waiting-tint text-state-waiting-ink",
};
/** The ring while something is under way carries the state's colour. */
const markStyles: Record<StateTone, string> = {
  done: "text-state-done-ink",
  working: "text-state-working-ink",
  neutral: "text-ink-3",
  failed: "text-state-failed-ink",
  waiting: "text-state-waiting-ink",
};
const sizeStyles = {
  /** A state beside a name, in a row or a summary. */
  default: "gap-1 text-caption-1-regular",
  /** Caption: option metadata in a list. */
  small: "gap-1 text-caption-1-regular",
} as const;

/**
 * A state as a badge: its word in a 20px pill. Colour says what the state
 * is: done green, waiting orange, failed red (their tint and ink); a fact
 * (a role, a kind) and work under way are grey on the surface's fill. While something
 * is under way a turning ring leads the word, so colour is never the only
 * clue. A word that is no state (a role, a kind) is the same badge. It takes
 * the 20px of a name's line, so a row is as tall with a state as without one.
 * The one state mark for settings, capabilities, computers, virtual
 * machines and Routines; the Controller's shared status values go through its `Tag`,
 * and the machine page's Plugins uses it too.
 */
export function StateTag({
  tone = "neutral",
  busy = false,
  size = "default",
  children,
  className,
  ...rest
}: {
  tone?: StateTone | undefined;
  /** Something is under way: a spinning ring leads the word. */
  busy?: boolean;
  size?: keyof typeof sizeStyles;
  children: ReactNode;
  className?: string;
} & HTMLAttributes<HTMLSpanElement>) {
  return (
    <span
      data-state-badge={tone}
      {...rest}
      className={cx(
        "inline-flex h-5 max-w-full shrink-0 items-center rounded-full px-2 whitespace-nowrap",
        sizeStyles[size],
        badgeStyles[tone],
        // Where a row has no room left even for its name at the minimum
        // (`useStateFit`), the badge is a dot in its own ink: its word stays
        // for assistive technology and in the row's `title`.
        "group-data-[compact]/state:size-2 group-data-[compact]/state:min-w-0 group-data-[compact]/state:overflow-hidden group-data-[compact]/state:bg-current group-data-[compact]/state:px-0",
        className,
      )}
    >
      {busy && (
        <AnimatedIcon
          name="loader-circle"
          working
          className={cx(
            "size-3.5 shrink-0 group-data-[compact]/state:hidden",
            markStyles[tone],
          )}
        />
      )}
      <span className="min-w-0 truncate group-data-[compact]/state:sr-only">
        {children}
      </span>
    </span>
  );
}
