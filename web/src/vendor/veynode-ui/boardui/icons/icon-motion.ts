import type { IconNodeName } from "./icon-nodes";

/**
 * The motions an icon can carry (ds 「图标动效」) and the rules that decide,
 * without React, what an AnimatedIcon shows. The drawings are in
 * animated-icon.tsx and the keyframes in packages/ui/src/styles/base.css
 * ("Icon motion").
 *
 * Triggers:
 *   play     a counter: bump it each time the action happens; mounting never plays
 *   on       a state; a change animates both ways, mounting shows it still
 *   hover    the enclosing button, link, menu item or `.icon-motion-host` is
 *            hovered or keyboard-focused (or `hover` forces it)
 *   working  the only loop; stopping lets a turn finish, or settles in 160ms
 */
export type IconMotion =
  // State (`on`).
  | "copied"
  | "play"
  | "theme"
  | "flip"
  | "menu"
  | "plus"
  | "blink"
  | "slash"
  | "unlock"
  | "mute"
  | "panel"
  // Hover (`folder` also takes `on`, for a drop target under a dragged file).
  | "folder"
  | "turn"
  | "lid"
  | "search"
  | "out"
  | "reply"
  // Once per action (`play`).
  | "send"
  | "refresh"
  | "drop"
  | "lift"
  | "pin"
  | "ring"
  | "power"
  | "lang"
  // While working.
  | "twinkle";

export type IconMotionTrigger = "on" | "hover" | "play" | "working";

export const MOTION_TRIGGER: Readonly<Record<IconMotion, IconMotionTrigger>> = {
  copied: "on",
  play: "on",
  theme: "on",
  flip: "on",
  menu: "on",
  plus: "on",
  blink: "on",
  slash: "on",
  unlock: "on",
  mute: "on",
  panel: "on",
  folder: "hover",
  turn: "hover",
  lid: "hover",
  search: "hover",
  out: "hover",
  reply: "hover",
  send: "play",
  refresh: "play",
  drop: "play",
  lift: "play",
  pin: "play",
  ring: "play",
  power: "play",
  lang: "play",
  twinkle: "working",
};

/** The motion an icon carries unless `motion` says otherwise. */
export const MOTION_OF: Readonly<Partial<Record<IconNodeName, IconMotion>>> = {
  copy: "copied",
  play: "play",
  sun: "theme",
  folder: "folder",
  "chevron-down": "flip",
  "chevron-right": "flip",
  "chevron-up": "flip",
  menu: "menu",
  plus: "plus",
  eye: "blink",
  mic: "slash",
  video: "slash",
  lock: "unlock",
  "volume-2": "mute",
  "panel-right": "panel",
  settings: "turn",
  "trash-2": "lid",
  search: "search",
  "external-link": "out",
  reply: "reply",
  "arrow-up": "send",
  "refresh-cw": "refresh",
  "rotate-cw": "refresh",
  "rotate-ccw": "refresh",
  "loader-circle": "refresh",
  download: "drop",
  upload: "lift",
  pin: "pin",
  bell: "ring",
  power: "power",
  languages: "lang",
  sparkles: "twinkle",
};

/**
 * The lucide name the icon shows now, for its `lucide-<name>` class: a
 * copy that landed is a check, a playing button shows pause, and so on.
 * Hooks such as `svg.lucide-check` keep meaning what they meant.
 */
export function shownIcon(name: IconNodeName, motion: IconMotion | undefined, on: boolean): string {
  if (!on) return name;
  switch (motion) {
    case "copied":
      return "check";
    case "play":
      return "pause";
    case "theme":
      return "moon";
    case "folder":
      return "folder-open";
    case "blink":
    case "slash":
      return `${name}-off`;
    case "unlock":
      return "lock-open";
    case "mute":
      return "volume-x";
    case "panel":
      return "panel-right-open";
    default:
      return name;
  }
}

export type MotionInputs = {
  play: number | undefined;
  on: boolean;
  working: boolean;
  low: boolean;
};

/**
 * What the icon mounted with, so that only a change made in front of the
 * person animates: `live` once anything changed since mount (a remount, a
 * conversation from history or a reload show the state without motion), and
 * `playing` once `play` moved away from its first value (each new value
 * restarts the one-shot).
 */
export function createMotionMemory(initial: MotionInputs) {
  let live = false;
  return {
    see(next: MotionInputs): { live: boolean; playing: boolean } {
      if (
        !live &&
        (next.play !== initial.play ||
          next.on !== initial.on ||
          next.working !== initial.working ||
          next.low !== initial.low)
      )
        live = true;
      return {
        live,
        playing: next.play !== undefined && next.play !== initial.play,
      };
    },
  };
}

/** One spin of a working loader (ds: one turn per second, linear). */
export const SPIN_TURN_MS = 1000;

/**
 * How a loop that is told to stop comes to rest: a spinner finishes the
 * turn it is in (never snapping back), sparkles settle in 160ms, and with
 * reduced motion there is no loop to finish.
 */
export function stopOf(motion: IconMotion | undefined, reduced: boolean): "turn" | "settle" | "now" {
  if (reduced) return "now";
  if (motion === "twinkle") return "settle";
  if (motion === "refresh") return "turn";
  return "now";
}

export function iconMotionClasses({
  name,
  motion,
  on,
  hover,
  low,
  live,
  playing,
  spinning,
  working,
  settled,
}: {
  name: IconNodeName;
  motion: IconMotion | undefined;
  on: boolean;
  hover: boolean;
  low: boolean;
  live: boolean;
  playing: boolean;
  spinning: boolean;
  working: boolean;
  settled: boolean;
}): string {
  const classes = ["lucide", `lucide-${shownIcon(name, motion, on)}`, "icon-motion"];
  if (motion) classes.push(`im-${motion}`);
  if (name === "chevron-right") classes.push("im-r90");
  if (name === "rotate-ccw") classes.push("im-ccw");
  if (live) classes.push("im-live");
  if (on) classes.push("im-on");
  if (hover) classes.push("im-hover");
  if (low) classes.push("im-low");
  if (playing) classes.push("im-play");
  if (spinning) classes.push("im-working");
  if (settled && !working) classes.push("im-settle");
  return classes.join(" ");
}
