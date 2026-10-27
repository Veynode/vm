import { useEffect, useRef } from "react";
import { MARKS } from "./logo-marks";

/** The product's name: a name, not a translation. */
const PRODUCT = "VeynodeVM";

/** The antenna (its stalk and bead) in a group of its own, pivoting where it meets the head, so it can wave. */
export const withAntenna = (svg: string) =>
  svg.replace(
    /<path d="M38 14\.5[^>]*\/><circle cx="46"[^>]*\/>/,
    (antenna) => `<g data-logo-antenna style="transform-box:view-box;transform-origin:38px 15px">${antenna}</g>`,
  );
/** The welcome wave: 200ms, as the mark appears, once. */
const WAVE: Keyframe[] = [
  { transform: "none", offset: 0 },
  { transform: "rotate(-16deg)", offset: 0.2 },
  { transform: "rotate(12deg)", offset: 0.45 },
  { transform: "rotate(-5deg)", offset: 0.7 },
  { transform: "none", offset: 1 },
];

/**
 * Veynode's mark, and with `wordmark` the name beside it in Inter 700 at
 * 0.56 of the mark's height, 0.28 of it apart. Below 28px the mark drops its
 * antenna. `tone="mono"` draws it in one ink that follows the text colour.
 * The mark alone is an image named Veynode, unless `decorative` (text beside
 * it already names the product); with the wordmark the name is the text.
 * `wave`: the Bot in the mark waves its antenna once when it appears (the
 * welcome and sign-in screens); never with reduced motion.
 */
export function Logo({
  size = 32,
  wordmark = false,
  tone = "color",
  decorative = false,
  wave = false,
  className = "",
}: {
  size?: number;
  wordmark?: boolean;
  tone?: "color" | "mono";
  decorative?: boolean;
  wave?: boolean;
  className?: string;
}) {
  const inner = withAntenna(tone === "mono" ? MARKS.mono : size < 28 ? MARKS.small : MARKS.mark);
  const mark = useRef<SVGSVGElement>(null);
  // Played when the mark mounts, not when it redraws.
  useEffect(() => {
    if (!wave || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const antenna = mark.current?.querySelector<SVGGElement>("[data-logo-antenna]");
    const animation = antenna?.animate(WAVE, { duration: 200, easing: "cubic-bezier(.2,0,0,1)" });
    return () => animation?.cancel();
  }, [wave]);
  return (
    <span
      data-logo
      className={"inline-flex shrink-0 items-center leading-none " + className}
      style={{ gap: size * 0.28 }}
    >
      <svg
        ref={mark}
        viewBox="0 0 64 64"
        width={size}
        height={size}
        className="block shrink-0"
        {...(wordmark || decorative ? { "aria-hidden": true } : { role: "img", "aria-label": PRODUCT })}
        focusable="false"
        dangerouslySetInnerHTML={{ __html: inner }}
      />
      {wordmark && (
        <span
          className="font-bold tracking-[-0.02em] text-ink"
          style={{ fontSize: size * 0.56 }}
        >
          {PRODUCT}
        </span>
      )}
    </span>
  );
}
