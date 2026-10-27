import type { ReactNode } from "react";
import type { AvatarColorName } from "@veynode/shared/avatar";
import styles from "./ComputerIcon.module.css";
import type { ComputerIconKind } from "./computer-icon";

/**
 * The drawings of a computer's icon (ds 电脑图标, 贴纸): flat devices on a 64
 * grid, each cut out along `sil`. The colour is in what the device is made
 * of or shows; bezels, stands and keyboards stay grey.
 */

/** A colour as the drawings use it: body, shade, light, and the ink that reads on the body. */
type StickerPaint = readonly [
  body: string,
  shade: string,
  light: string,
  ink: string,
];

export const stickerPaints: Readonly<Record<AvatarColorName, StickerPaint>> = {
  blue: ["#328cff", "#1f6fe0", "#bcd8ff", "#fff"],
  teal: ["#20bdb0", "#149c91", "#a8ebe5", "#fff"],
  green: ["#00c879", "#00a562", "#a3f0cc", "#fff"],
  yellow: ["#ffb337", "#e8931a", "#ffd596", "#3a2800"],
  orange: ["#ff7a18", "#e25f00", "#ffc49a", "#fff"],
  pink: ["#f653b5", "#d83a98", "#ffc2e6", "#fff"],
  purple: ["#a576ff", "#8454ea", "#d2c0ff", "#fff"],
  black: ["#3a3f4a", "#23262e", "#8b92a0", "#fff"],
};
/** A computer that is stopped or cannot be reached: unlit, whatever its colour. */
export const STICKER_OFF: StickerPaint = [
  "#5a606c",
  "#454a55",
  "#7b8290",
  "#aab1bd",
];

type StickerState = { off: boolean; work: boolean };
type Sticker = {
  /** How far it leans, in degrees: fixed per kind, so a list does not shuffle. */
  tilt: number;
  /** Its colour when none was chosen. */
  color: AvatarColorName;
  /** The outline it is cut along. */
  sil: string;
  art: (paint: StickerPaint, state: StickerState) => ReactNode;
};

const BEZEL = "#2b2f39";
const shine = ({ off }: StickerState) => (off ? 0.2 : 0.6);
/** The lit region breathing while a Bot works. */
const glow = (state: StickerState, node: ReactNode) =>
  state.work ? (
    <g className={styles["glow"]} fill="#fff">
      {node}
    </g>
  ) : null;
const led = ({ work }: StickerState) => (work ? styles["led"] : undefined);

const WINDOW =
  "M14 11h36a7 7 0 0 1 7 7v28a7 7 0 0 1-7 7H14a7 7 0 0 1-7-7V18a7 7 0 0 1 7-7Z";
/** A window with a title bar and two dots, and `marks` in ink on it. */
const windowArt =
  (marks: string, cursor?: string) =>
  ([m, s, , ink]: StickerPaint, state: StickerState) => (
    <>
      <path d={WINDOW} fill={m} />
      {glow(state, <path d={WINDOW} />)}
      <path d="M7 18a7 7 0 0 1 7-7h36a7 7 0 0 1 7 7v5H7Z" fill={s} />
      <circle cx={13.5} cy={17} r={1.7} fill="#fff" />
      <circle cx={19} cy={17} r={1.7} fill="#fff" />
      {!state.off && (
        <g
          fill="none"
          stroke={ink}
          strokeWidth={3}
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <path d={marks} />
          {cursor && (
            <path
              d={cursor}
              className={state.work ? styles["cursor"] : undefined}
            />
          )}
        </g>
      )}
    </>
  );

const MINI =
  "M14 20h36a9 9 0 0 1 9 9v10a9 9 0 0 1-9 9H14a9 9 0 0 1-9-9V29a9 9 0 0 1 9-9Z";
const CLOUD =
  "M20 49a12.5 12.5 0 0 1-2-24.8A15.5 15.5 0 0 1 47.3 21 14 14 0 0 1 45 49Z";
const CHIP =
  "M19 10h26a9 9 0 0 1 9 9v26a9 9 0 0 1-9 9H19a9 9 0 0 1-9-9V19a9 9 0 0 1 9-9Z";
const FOLDER =
  "M13 12h12a5 5 0 0 1 3.9 1.9L32 18h19a6 6 0 0 1 6 6v22a6 6 0 0 1-6 6H13a6 6 0 0 1-6-6V18a6 6 0 0 1 6-6Z";

export const STICKERS: Readonly<Record<ComputerIconKind, Sticker>> = {
  laptop: {
    tilt: -6,
    color: "blue",
    sil: "M16 13h32a4 4 0 0 1 4 4v23h5v3a5 5 0 0 1-5 5H12a5 5 0 0 1-5-5v-3h5V17a4 4 0 0 1 4-4Z",
    art: ([m, s], state) => (
      <>
        <path
          d="M16 13h32a4 4 0 0 1 4 4v23H12V17a4 4 0 0 1 4-4Z"
          fill={BEZEL}
        />
        <rect x={15.5} y={16.5} width={33} height={20.5} rx={2} fill={m} />
        <path d="M48.5 21v16h-24Z" fill={s} />
        {glow(
          state,
          <rect x={15.5} y={16.5} width={33} height={20.5} rx={2} />,
        )}
        <rect
          x={19}
          y={20}
          width={8}
          height={2.4}
          rx={1.2}
          fill="#fff"
          opacity={shine(state)}
        />
        <path d="M7 40h50v3a5 5 0 0 1-5 5H12a5 5 0 0 1-5-5Z" fill="#dfe3e9" />
        <path d="M7 44h50a5 5 0 0 1-5 4H12a5 5 0 0 1-5-4Z" fill="#b9bfc9" />
        <path d="M26 40h12v1a2 2 0 0 1-2 2h-8a2 2 0 0 1-2-2Z" fill="#9aa1ad" />
      </>
    ),
  },
  monitor: {
    tilt: 5,
    color: "teal",
    sil: "M12 9h40a5 5 0 0 1 5 5v25a5 5 0 0 1-5 5H38l1.5 7H44a2 2 0 0 1 0 4H20a2 2 0 0 1 0-4h4.5l1.5-7H12a5 5 0 0 1-5-5V14a5 5 0 0 1 5-5Z",
    art: ([m, s], state) => (
      <>
        <path
          d="M26 43h12l2 9h4a2 2 0 0 1 0 4H20a2 2 0 0 1 0-4h4Z"
          fill="#c9ced6"
        />
        <path d="M18 54h28a2 2 0 0 1-2 2H20a2 2 0 0 1-2-2Z" fill="#a6adb9" />
        <rect x={7} y={9} width={50} height={35} rx={5} fill={BEZEL} />
        <rect x={10.5} y={12.5} width={43} height={24} rx={2.5} fill={m} />
        <path d="M53.5 18v18.5h-27Z" fill={s} />
        {glow(
          state,
          <rect x={10.5} y={12.5} width={43} height={24} rx={2.5} />,
        )}
        <rect
          x={14}
          y={16}
          width={9}
          height={2.4}
          rx={1.2}
          fill="#fff"
          opacity={shine(state)}
        />
        <circle cx={32} cy={40.3} r={1.3} fill="#8b92a0" />
      </>
    ),
  },
  // A container, or this machine without a desktop of its own: a small host.
  box: {
    tilt: -4,
    color: "green",
    sil: MINI,
    art: ([m, s, l, ink], state) => (
      <>
        <rect x={5} y={20} width={54} height={28} rx={9} fill={m} />
        <path d="M5 36h54v3a9 9 0 0 1-9 9H14a9 9 0 0 1-9-9Z" fill={s} />
        {glow(state, <path d={MINI} />)}
        <rect
          x={12}
          y={25}
          width={10}
          height={2.6}
          rx={1.3}
          fill="#fff"
          opacity={shine(state)}
        />
        <circle className={led(state)} cx={50} cy={42} r={2} fill={ink} />
        <path
          d="M13 42h14"
          stroke={l}
          strokeWidth={2.4}
          strokeLinecap="round"
        />
      </>
    ),
  },
  server: {
    tilt: 4,
    color: "purple",
    sil: "M15 7h34a6 6 0 0 1 6 6v38a6 6 0 0 1-6 6H15a6 6 0 0 1-6-6V13a6 6 0 0 1 6-6Z",
    art: ([m, s, l, ink], state) => (
      <>
        <rect x={9} y={7} width={46} height={50} rx={6} fill={m} />
        {[12, 27, 42].map((y, i) => (
          <g key={y}>
            <rect x={13} y={y} width={38} height={11} rx={3.5} fill={s} />
            <circle
              className={led(state)}
              style={state.work ? { animationDelay: `${i * 0.4}s` } : undefined}
              cx={19}
              cy={y + 5.5}
              r={2}
              fill={ink}
            />
            <path
              d={`M29 ${y + 5.5}h16`}
              stroke={l}
              strokeWidth={2.4}
              strokeLinecap="round"
            />
          </g>
        ))}
      </>
    ),
  },
  vm: {
    tilt: -5,
    color: "yellow",
    sil: "M22 8h29a6 6 0 0 1 6 6v23a6 6 0 0 1-6 6h-3v6a6 6 0 0 1-6 6H13a6 6 0 0 1-6-6V26a6 6 0 0 1 6-6h3v-6a6 6 0 0 1 6-6Z",
    art: ([m, s, l, ink], state) => (
      <>
        <rect x={16} y={8} width={41} height={35} rx={6} fill={l} />
        <rect x={7} y={20} width={41} height={35} rx={6} fill={m} />
        {glow(state, <rect x={7} y={20} width={41} height={35} rx={6} />)}
        <path d="M7 26a6 6 0 0 1 6-6h29a6 6 0 0 1 6 6v4H7Z" fill={s} />
        <circle cx={13.5} cy={25} r={1.7} fill="#fff" />
        <circle cx={19} cy={25} r={1.7} fill="#fff" />
        {!state.off && (
          <path
            d="M15 38l5 4-5 4 M25 46h10"
            fill="none"
            stroke={ink}
            strokeWidth={3}
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        )}
      </>
    ),
  },
  // A machine somewhere else: a cloud host.
  globe: {
    tilt: 6,
    color: "blue",
    sil: CLOUD,
    art: ([m, s, , ink], state) => (
      <>
        <path d={CLOUD} fill={m} />
        <path
          d="M8.6 41h49.6A14 14 0 0 1 45 49H20a12.5 12.5 0 0 1-11.4-8Z"
          fill={s}
        />
        {glow(state, <path d={CLOUD} />)}
        <rect
          x={24}
          y={20}
          width={9}
          height={2.6}
          rx={1.3}
          fill="#fff"
          opacity={shine(state)}
          transform="rotate(-25 28 21)"
        />
        <path
          d="M24 36h16"
          stroke={ink}
          strokeWidth={2.8}
          strokeLinecap="round"
        />
        <circle className={led(state)} cx={24} cy={44.5} r={1.7} fill={ink} />
        <circle cx={30} cy={44.5} r={1.7} fill={ink} />
      </>
    ),
  },
  terminal: {
    tilt: -4,
    color: "black",
    sil: WINDOW,
    art: windowArt("M16 32l6 5-6 5", "M28 43h11"),
  },
  code: {
    tilt: 4,
    color: "pink",
    sil: WINDOW,
    art: windowArt("M25 31l-6 6 6 6 M39 31l6 6-6 6 M34.5 29l-5 16"),
  },
  cpu: {
    tilt: -5,
    color: "orange",
    sil: CHIP,
    art: ([m, s, l], state) => (
      <>
        <path
          d="M24 10v44 M32 10v44 M40 10v44 M10 24h44 M10 32h44 M10 40h44"
          stroke="#9aa1ad"
          strokeWidth={3.4}
        />
        <rect x={15} y={15} width={34} height={34} rx={7} fill={m} />
        <path d="M15 34h34v8a7 7 0 0 1-7 7H22a7 7 0 0 1-7-7Z" fill={s} />
        <rect x={24.5} y={24.5} width={15} height={15} rx={4} fill={l} />
        {glow(state, <rect x={24.5} y={24.5} width={15} height={15} rx={4} />)}
        <rect
          x={19}
          y={19}
          width={8}
          height={2.4}
          rx={1.2}
          fill="#fff"
          opacity={shine(state)}
        />
      </>
    ),
  },
  folder: {
    tilt: -5,
    color: "yellow",
    sil: FOLDER,
    art: ([m, s], state) => (
      <>
        <path d={FOLDER} fill={s} />
        <rect x={7} y={24} width={50} height={28} rx={6} fill={m} />
        {glow(state, <rect x={7} y={24} width={50} height={28} rx={6} />)}
        <rect
          x={13}
          y={29}
          width={10}
          height={2.6}
          rx={1.3}
          fill="#fff"
          opacity={shine(state)}
        />
      </>
    ),
  },
};
