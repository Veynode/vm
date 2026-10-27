import type { ReactNode } from "react";

/**
 * From this size up a drawing is a sticker (ds 贴纸): cut out with a white
 * edge, leaning, with a shadow under it. Smaller, the edge would blur the
 * drawing, so it is drawn flat.
 */
const STICKER_MIN_SIZE = 28;
export const isSticker = (size: number) => size >= STICKER_MIN_SIZE;

/**
 * A drawing on the 64 grid as a sticker, or flat below `STICKER_MIN_SIZE`:
 * what goes inside an `<svg viewBox="0 0 64 64" overflow="visible">`.
 */
export function StickerArt({
  id,
  sil,
  tilt,
  size,
  dark,
  dim = false,
  sticker = isSticker(size),
  children,
}: {
  /** Unique within the document: it names the sticker's clip and shadow. */
  id: string;
  /** The outline it is cut along. */
  sil: string;
  /** How far it leans, in degrees. */
  tilt: number;
  size: number;
  dark: boolean;
  /** Unlit: the band of light across it fades. */
  dim?: boolean;
  /** A sticker at any size: a mark on another sticker matches it. */
  sticker?: boolean;
  children: ReactNode;
}) {
  if (!sticker)
    return (
      <g transform="translate(32 32) scale(1.14) translate(-32 -32)">
        {children}
      </g>
    );
  return (
    <>
      <defs>
        <clipPath id={`${id}c`}>
          <path d={sil} />
        </clipPath>
        <filter id={`${id}f`} x="-20%" y="-20%" width="140%" height="150%">
          <feDropShadow
            dx={0}
            dy={1.6}
            stdDeviation={1.4}
            floodColor="#0b0d12"
            floodOpacity={dark ? 0.6 : 0.26}
          />
        </filter>
      </defs>
      <g
        transform={`rotate(${tilt} 32 32) translate(4.5 4.5) scale(.86)`}
        data-sticker=""
      >
        {/* The die-cut edge: the outline, grown and white. */}
        <path
          d={sil}
          fill="#fff"
          stroke="#fff"
          strokeWidth={7.5}
          strokeLinejoin="round"
          filter={`url(#${id}f)`}
        />
        <g clipPath={`url(#${id}c)`}>
          {children}
          {/* One band of light across it. */}
          <path
            d="M-6 34L42 -4h9L-6 44Z"
            fill="#fff"
            opacity={dim ? 0.08 : 0.16}
          />
        </g>
      </g>
    </>
  );
}
