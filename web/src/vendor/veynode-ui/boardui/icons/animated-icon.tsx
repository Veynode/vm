import {
  createElement,
  useEffect,
  useId,
  useRef,
  useState,
  type AnimationEvent,
  type ReactNode,
  type SVGProps,
} from "react";
import { cx } from "../utils/cx";
import { ICON_NODES, type IconNode, type IconNodeName } from "./icon-nodes";
import {
  createMotionMemory,
  iconMotionClasses,
  MOTION_OF,
  MOTION_TRIGGER,
  stopOf,
  SPIN_TURN_MS,
  type IconMotion,
} from "./icon-motion";

export type { IconMotion } from "./icon-motion";
export type { IconNodeName as AnimatedIconName } from "./icon-nodes";
// The rules and drawings, for tests that check them without a DOM.
export {
  createMotionMemory,
  iconMotionClasses,
  MOTION_OF,
  MOTION_TRIGGER,
  shownIcon,
  stopOf,
} from "./icon-motion";
export { ICON_NODES } from "./icon-nodes";

/**
 * An icon that moves when its own action happens or its state changes (ds
 * 「图标动效」), never by itself; only `working` loops. The drawing is the
 * lucide icon split into <g> parts, and at rest it draws exactly what the
 * plain lucide icon draws, with the same `lucide lucide-<name>` classes, so
 * it sizes, colours and presses like any other icon. The parts move by
 * transform, opacity or stroke-dash only (packages/ui/src/styles/base.css,
 * "Icon motion"). Reduced motion: nothing moves or scales; a state is simply
 * shown, and a swap or slash fades in 120ms at most.
 *
 *   <AnimatedIcon name="arrow-up" play={sent} />       bump `sent` per send
 *   <AnimatedIcon name="copy" on={copied} />            copy → check
 *   <AnimatedIcon name="chevron-down" on={open} />
 *   <AnimatedIcon name="loader-circle" working />
 *   <AnimatedIcon name="settings" />                    turns on the button's hover
 */
export interface AnimatedIconProps
  extends Omit<SVGProps<SVGSVGElement>, "children" | "ref" | "name"> {
  name: IconNodeName;
  /** Another icon's motion, e.g. `slash` for any icon (defaults to the icon's own). */
  motion?: IconMotion | undefined;
  /** Bumped each time the action happens; the value it mounts with never plays. */
  play?: number | undefined;
  /** A state (open, copied, muted, shown…); only a change after mount animates. */
  on?: boolean | undefined;
  /** Forces a hover motion, e.g. while something is dragged over the control. */
  hover?: boolean | undefined;
  /** The only loop: a refresh or load in progress, 帮我写 drafting. */
  working?: boolean | undefined;
  /** `mute` only: the outer wave folds away (a low volume). */
  low?: boolean | undefined;
  size?: number | string | undefined;
}

type Draw = (name: IconNodeName, mask: string) => ReactNode;

const nodes = (list: IconNode): ReactNode[] =>
  list.map(([tag, attributes], index) => createElement(tag, { key: index, ...attributes }));
const pick = (name: IconNodeName, indexes: readonly number[]): IconNode => {
  const list: IconNode = ICON_NODES[name];
  return indexes.flatMap((index) => {
    const element = list[index];
    return element ? [element] : [];
  });
};
const part = (className: string, list: IconNode, key: string = className): ReactNode => (
  <g key={key} className={className}>
    {nodes(list)}
  </g>
);
const drawn = (d: string, key: string): ReactNode => (
  <path key={key} d={d} className="im-dr" pathLength={1} />
);
/** A slash that draws across the icon and cuts a gap in it, like lucide's *-off icons. */
const slashed = (mask: string, base: ReactNode): ReactNode[] => [
  <mask key="m" id={mask} maskUnits="userSpaceOnUse" x={-2} y={-2} width={28} height={28}>
    <rect x={-2} y={-2} width={28} height={28} fill="#fff" stroke="none" />
    <path d="m2 2 20 20" className="im-dr" pathLength={1} stroke="#000" strokeWidth={4.5} />
  </mask>,
  <g key="b" mask={`url(#${mask})`}>
    {base}
  </g>,
  drawn("m2 2 20 20", "s"),
];
const whole: Draw = (name) => part("im-p", ICON_NODES[name]);
/** Hover motions move lucide's own children (the rules are keyed to the lucide class). */
const flat: Draw = (name) => nodes(ICON_NODES[name]);

const DRAW: Readonly<Record<IconMotion, Draw>> = {
  copied: (name) => [part("im-sw im-a", ICON_NODES[name]), part("im-sw im-b", ICON_NODES.check)],
  play: () => [part("im-sw im-a", ICON_NODES.play), part("im-sw im-b", ICON_NODES.pause)],
  theme: () => [part("im-sw im-a", ICON_NODES.sun), part("im-sw im-b", ICON_NODES.moon)],
  folder: () => [part("im-sw im-a", ICON_NODES.folder), part("im-sw im-b", ICON_NODES["folder-open"])],
  flip: whole,
  menu: () => [
    part("im-l1", pick("menu", [0])),
    part("im-l2", pick("menu", [1])),
    part("im-l3", pick("menu", [2])),
  ],
  plus: whole,
  blink: (name, mask) => slashed(mask, part("im-e", ICON_NODES[name])),
  slash: (name, mask) => slashed(mask, part("im-p", ICON_NODES[name])),
  unlock: () => [part("im-p", pick("lock", [0])), part("im-k", pick("lock", [1]))],
  mute: () => [
    part("im-p", pick("volume-2", [0])),
    part("im-w1", pick("volume-2", [1])),
    part("im-w2", pick("volume-2", [2])),
    <g key="x" className="im-x">
      {drawn("M22 9 16 15", "a")}
      {drawn("M16 9 22 15", "b")}
    </g>,
  ],
  // At rest this is lucide's panel-right exactly; open, the pane's edge
  // slides 3px left and the pane fills.
  panel: () => [
    part("im-p", pick("panel-right", [0])),
    <g key="f" className="im-fill">
      <path d="M12 3h7a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-7z" fill="currentColor" fillOpacity={0.16} stroke="none" />
    </g>,
    part("im-bar", pick("panel-right", [1])),
  ],
  turn: flat,
  lid: flat,
  search: flat,
  out: flat,
  reply: flat,
  send: whole,
  refresh: whole,
  drop: () => [part("im-p", pick("download", [1])), part("im-ar", pick("download", [0, 2]))],
  lift: () => [part("im-ar", pick("upload", [0, 1])), part("im-p", pick("upload", [2]))],
  pin: whole,
  ring: () => [part("im-p", pick("bell", [1])), part("im-c", pick("bell", [0]))],
  // lucide's power, each stroke with a unit length to draw it on.
  power: () => [
    <g key="s" className="im-st">
      <path d="M12 2v10" pathLength={1} />
    </g>,
    <g key="r" className="im-rg">
      <path d="M18.4 6.6a9 9 0 1 1-12.77.04" pathLength={1} />
    </g>,
  ],
  lang: () => [part("im-l", pick("languages", [0, 1, 2, 3])), part("im-r", pick("languages", [4, 5]))],
  twinkle: () => [
    part("im-st", pick("sparkles", [0])),
    part("im-s1", pick("sparkles", [1, 2])),
    part("im-s2", pick("sparkles", [3])),
  ],
};

const reducedMotion = () =>
  typeof window !== "undefined" &&
  typeof window.matchMedia === "function" &&
  window.matchMedia("(prefers-reduced-motion: reduce)").matches;

export function AnimatedIcon({
  name,
  motion,
  play,
  on = false,
  hover = false,
  working = false,
  low = false,
  size = 20,
  className,
  onAnimationIteration,
  onAnimationEnd,
  ...rest
}: AnimatedIconProps) {
  const chosen = motion ?? MOTION_OF[name];
  const mask = `im${useId().replace(/[^a-zA-Z0-9]/g, "")}`;
  const memory = useRef<ReturnType<typeof createMotionMemory> | null>(null);
  memory.current ??= createMotionMemory({ play, on, working, low });
  const { live, playing } = memory.current.see({ play, on, working, low });
  // A loop keeps its class while it finishes the turn it is in.
  const [spinning, setSpinning] = useState(working);
  const [settled, setSettled] = useState(false);
  useEffect(() => {
    if (working) {
      setSpinning(true);
      return;
    }
    if (!spinning) return;
    const stop = stopOf(chosen, reducedMotion());
    if (stop === "turn") {
      // The turn's end stops it (below); this only covers a spin that never
      // reports one (a hidden tab, an animation the page turned off).
      const timer = setTimeout(() => setSpinning(false), SPIN_TURN_MS + 100);
      return () => clearTimeout(timer);
    }
    setSpinning(false);
    if (stop === "settle") setSettled(true);
    return undefined;
  }, [working, spinning, chosen]);
  const endTurn = (event: AnimationEvent<SVGSVGElement>) => {
    if (!working && event.animationName === "icon-motion-spin") setSpinning(false);
  };
  const trigger = chosen ? MOTION_TRIGGER[chosen] : undefined;
  return (
    <svg
      key={playing ? `play-${String(play)}` : "rest"}
      xmlns="http://www.w3.org/2000/svg"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={cx(
        iconMotionClasses({
          name,
          motion: chosen,
          on,
          hover: hover && trigger === "hover",
          low,
          live,
          playing,
          spinning: spinning && (working || stopOf(chosen, false) === "turn"),
          working,
          settled,
        }),
        className,
      )}
      onAnimationIteration={(event) => {
        endTurn(event);
        onAnimationIteration?.(event);
      }}
      onAnimationEnd={(event) => {
        endTurn(event);
        onAnimationEnd?.(event);
      }}
      {...rest}
    >
      {chosen ? DRAW[chosen](name, mask) : nodes(ICON_NODES[name])}
    </svg>
  );
}
