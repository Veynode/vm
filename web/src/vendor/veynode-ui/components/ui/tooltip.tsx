import { Tooltip as TooltipPrimitive } from "@base-ui/react/tooltip";
import { createContext, useContext, useId, useState } from "react";
import type { ReactNode } from "react";
import { sortCx } from "../../boardui/utils/cx";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Tooltip (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: a small solid card that floats (the `float`
 * shadow), the same surface a menu has.
 *   surface  `TOOLTIP_SURFACE`: bg surface, text ink, radius md; at most
 *            240px wide; one or two short sentences. A card that explains at
 *            more length (`DetailTip`) wears the same surface and inset.
 *   sizes    sm: `TOOLTIP_INSET` (px 12 py 8), caption (12px, 500)
 *            md: px 14 py 10, body (14px, 500)
 *   arrow    off by default; `showArrow` draws a filled caret in the
 *            surface's colour. A distinct path per side (not one path rotated
 *            with CSS) faces the trigger, see `TOOLTIP_CARETS`.
 *   motion   fades in and rises 3px over 120ms; under reduced motion it
 *            only fades.
 *   layer    `z-menu`: over a dialog.
 *
 * Hover waits 400ms the first time; once one tooltip is up its neighbours
 * open at once (mount one `TooltipProvider` near the root for that). Focus
 * opens at once, Escape closes. The card is non-interactive, so moving off
 * the trigger never keeps it open and its words cannot be selected by
 * accident. A press does not close it.
 *
 *   <Tooltip>
 *     <TooltipTrigger render={<IconButton icon={Copy} aria-label="Copy" />} />
 *     <TooltipContent>Copy</TooltipContent>
 *   </Tooltip>
 *
 * `render` puts the trigger's behaviour on your own element (an icon button,
 * a `<span tabIndex={0}>` around a disabled button); without it the trigger
 * is a plain `<button>`.
 */

export type TooltipSize = "sm" | "md";

/** What every tooltip and detail card is drawn on. */
export const TOOLTIP_SURFACE = "rounded-md bg-surface text-ink shadow-float";
/** The inset of its words, and their type. */
export const TOOLTIP_INSET = "px-3 py-2";
export const TOOLTIP_TEXT = "text-caption-1-medium";
/** The widest a tooltip or detail card grows. */
export const TOOLTIP_MAX_WIDTH = "max-w-[240px]";
/** The gap between the trigger and the card. */
export const TOOLTIP_OFFSET = 8;
/**
 * The first hover waits 400ms (ds Motion "Tooltip"); once one tooltip is up,
 * its neighbours open without waiting (the provider's shared warm-up). Focus
 * opens at once.
 */
export const TOOLTIP_DELAY_MS = 400;
/** What a trigger does unless told otherwise: wait 400ms, stay open on a press. */
export const TOOLTIP_TRIGGER_DEFAULTS = {
  delay: TOOLTIP_DELAY_MS,
  closeOnClick: false,
} as const;
/**
 * Fades in and rises 3px over 120ms, and leaves the same way. Base UI stamps
 * `data-starting-style` on mount and `data-ending-style` while it leaves, and
 * keeps the card mounted until the transition ends. The `entering` / `exiting`
 * selectors serve `DetailTip`, which is still a React Aria popover.
 */
export const TOOLTIP_MOTION = [
  "transition-[opacity,translate] duration-[120ms] ease-standard motion-reduce:translate-y-0",
  "data-[starting-style]:translate-y-[3px] data-[starting-style]:opacity-0",
  "data-[ending-style]:translate-y-[3px] data-[ending-style]:opacity-0 data-[ending-style]:ease-standard",
  "data-[entering]:translate-y-[3px] data-[entering]:opacity-0",
  "data-[exiting]:translate-y-[3px] data-[exiting]:opacity-0 data-[exiting]:ease-standard",
].join(" ");

const sizes = sortCx({
  sm: `${TOOLTIP_INSET} ${TOOLTIP_TEXT}`,
  md: "px-3 py-2 text-body-medium",
});

/**
 * One hand-drawn caret per side instead of a single path rotated with CSS
 * `transform`. Rotating a non-square (12x7) SVG changes its visual footprint
 * but not its layout box, which leaves a gap between the rotated triangle and
 * the card for `left`/`right`. Each entry has the `width`/`height` of its own
 * true footprint, so the layout box is exactly right: no transform, no gap.
 * `edge` puts it on the card's edge facing the trigger. The path is open on
 * the edge touching the card (no "Z").
 */
export const TOOLTIP_CARETS = sortCx({
  top: { width: 12, height: 7, path: "M0 0 L6 6 L12 0", edge: "top-full" },
  bottom: {
    width: 12,
    height: 7,
    path: "M0 7 L6 1 L12 7",
    edge: "bottom-full",
  },
  left: { width: 7, height: 12, path: "M0 0 L6 6 L0 12", edge: "left-full" },
  right: { width: 7, height: 12, path: "M7 0 L1 6 L7 12", edge: "right-full" },
});

type CaretSide = keyof typeof TOOLTIP_CARETS;

function caretSide(side: string): CaretSide {
  if (side === "inline-start") return "left";
  if (side === "inline-end") return "right";
  return side in TOOLTIP_CARETS ? (side as CaretSide) : "top";
}

function TooltipProvider({
  delay = TOOLTIP_DELAY_MS,
  closeDelay = 0,
  ...props
}: TooltipPrimitive.Provider.Props) {
  return (
    <TooltipPrimitive.Provider
      data-slot="tooltip-provider"
      delay={delay}
      closeDelay={closeDelay}
      {...props}
    />
  );
}

/** The card's id, while it is up: the trigger names it as its description. */
const CardContext = createContext<{ id: string; open: boolean } | undefined>(
  undefined,
);

function Tooltip({
  disableHoverablePopup = true,
  onOpenChange,
  ...props
}: TooltipPrimitive.Root.Props) {
  const id = useId();
  const [held, setHeld] = useState(props.defaultOpen ?? false);
  const open = props.open ?? held;
  return (
    <CardContext value={{ id, open }}>
      <TooltipPrimitive.Root
        data-slot="tooltip"
        disableHoverablePopup={disableHoverablePopup}
        onOpenChange={(next, details) => {
          setHeld(next);
          onOpenChange?.(next, details);
        }}
        {...props}
      />
    </CardContext>
  );
}

function TooltipTrigger({
  delay = TOOLTIP_TRIGGER_DEFAULTS.delay,
  closeOnClick = TOOLTIP_TRIGGER_DEFAULTS.closeOnClick,
  ...props
}: TooltipPrimitive.Trigger.Props) {
  const card = useContext(CardContext);
  return (
    <TooltipPrimitive.Trigger
      data-slot="tooltip-trigger"
      delay={delay}
      closeOnClick={closeOnClick}
      {...props}
      // While the card is up its words describe the trigger.
      aria-describedby={
        cn(props["aria-describedby"], card?.open && card.id) || undefined
      }
    />
  );
}

type TooltipContentProps = TooltipPrimitive.Popup.Props &
  Pick<
    TooltipPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset"
  > & {
    /** Surface size: `sm` is 12px caption, `md` 14px body. */
    size?: TooltipSize;
    /** The little caret pointing at the trigger. */
    showArrow?: boolean;
    /** Where the card is mounted; `body` by default. */
    container?: TooltipPrimitive.Portal.Props["container"];
    /** On the positioning layer, e.g. a `z-top` where `z-menu` is not enough. */
    positionerClassName?: string | undefined;
    children?: ReactNode;
  };

function TooltipContent({
  className,
  positionerClassName,
  side = "top",
  sideOffset = TOOLTIP_OFFSET,
  align = "center",
  alignOffset = 0,
  size = "sm",
  showArrow = false,
  container,
  children,
  ...props
}: TooltipContentProps) {
  const id = useContext(CardContext)?.id;
  return (
    <TooltipPrimitive.Portal container={container}>
      <TooltipPrimitive.Positioner
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        // A tip lasts a hover: it is placed once, not followed. Following asks
        // the browser to watch the trigger's size, and a transcript row that
        // resizes under a tip raised "ResizeObserver loop" page errors.
        disableAnchorTracking
        className={cn("z-menu", positionerClassName)}
      >
        <TooltipPrimitive.Popup
          data-slot="tooltip-content"
          role="tooltip"
          id={id}
          className={cn(
            "pointer-events-none relative select-none [overflow-wrap:anywhere]",
            TOOLTIP_MAX_WIDTH,
            TOOLTIP_SURFACE,
            sizes[size],
            TOOLTIP_MOTION,
            className,
          )}
          {...props}
        >
          {showArrow && (
            <TooltipPrimitive.Arrow
              data-slot="tooltip-arrow"
              render={(arrowProps, state) => {
                const caret = TOOLTIP_CARETS[caretSide(state.side)];
                return (
                  <div
                    {...arrowProps}
                    className={cn(arrowProps.className, "absolute", caret.edge)}
                    style={{
                      ...arrowProps.style,
                      width: caret.width,
                      height: caret.height,
                    }}
                  >
                    <svg
                      width={caret.width}
                      height={caret.height}
                      viewBox={`0 0 ${caret.width} ${caret.height}`}
                      className="block overflow-visible fill-surface"
                      aria-hidden
                    >
                      <path d={caret.path} />
                    </svg>
                  </div>
                );
              }}
            />
          )}
          {children}
        </TooltipPrimitive.Popup>
      </TooltipPrimitive.Positioner>
    </TooltipPrimitive.Portal>
  );
}

export { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger };
