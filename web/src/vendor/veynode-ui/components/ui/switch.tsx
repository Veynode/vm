import { Switch as SwitchPrimitive } from "@base-ui/react/switch";
import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Switch (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: an ink track when on, a line-strong track when off,
 * and a round thumb that stretches while held and lands on the pop curve (the
 * avatar's bounce). Flat: no gradients, highlights or inner chip.
 *
 *   size      track     thumb   travel
 *   compact   32×20     14      12
 *   sm        28×16     12      12
 *   md        38×22     16      16
 *   lg        52×30     24      22
 *
 * `shape="rectangle"` rounds the track to sm instead of full. The switch is
 * always inside a label, so a click on its words toggles it too.
 */
export type SwitchSize = "sm" | "md" | "lg" | "compact";
export type SwitchShape = "pill" | "rectangle";

const sizes = {
  compact: {
    track: "h-5 w-8",
    thumb:
      "h-3.5 w-3.5 left-[3px] top-[3px] data-checked:translate-x-3 group-active/switch:w-[18px] group-active/switch:data-checked:translate-x-2",
  },
  sm: {
    track: "h-4 w-7",
    thumb:
      "h-3 w-3 left-0.5 top-0.5 data-checked:translate-x-3 group-active/switch:w-[15px] group-active/switch:data-checked:translate-x-[9px]",
  },
  md: {
    track: "h-[22px] w-[38px]",
    thumb:
      "h-4 w-4 left-[3px] top-[3px] data-checked:translate-x-4 group-active/switch:w-5 group-active/switch:data-checked:translate-x-3",
  },
  lg: {
    track: "h-[30px] w-[52px]",
    thumb:
      "h-6 w-6 left-[3px] top-[3px] data-checked:translate-x-[22px] group-active/switch:w-7 group-active/switch:data-checked:translate-x-[18px]",
  },
} satisfies Record<SwitchSize, { track: string; thumb: string }>;

export type SwitchProps = Omit<
  SwitchPrimitive.Root.Props,
  "children" | "className"
> & {
  /** The words beside the switch. */
  children?: ReactNode;
  size?: SwitchSize;
  shape?: SwitchShape;
  /** On the label that holds the switch. */
  className?: string | undefined;
};

function Switch({
  className,
  children,
  size = "md",
  shape = "pill",
  ...props
}: SwitchProps) {
  const s = sizes[size];
  return (
    <label
      className={cn(
        "inline-flex items-center gap-2 select-none",
        props.disabled ? "cursor-not-allowed" : "cursor-pointer",
        className,
      )}
    >
      <SwitchPrimitive.Root
        data-slot="switch"
        className={cn(
          "group/switch relative shrink-0 bg-line-strong data-checked:bg-action",
          "transition-colors duration-200 ease-standard motion-reduce:transition-none",
          "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
          "data-disabled:cursor-not-allowed data-disabled:opacity-45",
          shape === "rectangle" ? "rounded-sm" : "rounded-full",
          s.track,
        )}
        {...props}
      >
        <SwitchPrimitive.Thumb
          data-slot="switch-thumb"
          className={cn(
            "absolute rounded-full bg-white data-checked:bg-on-action",
            // Travel lands on the pop curve (200ms); the squash while held is
            // quicker (120ms), as in the design's switch.
            "[transition:translate_200ms_cubic-bezier(0.3,1.4,0.5,1),width_120ms_cubic-bezier(0.2,0,0,1),background-color_200ms_cubic-bezier(0.2,0,0,1)] motion-reduce:transition-none",
            "group-data-disabled/switch:group-active/switch:w-auto",
            s.thumb,
          )}
        />
      </SwitchPrimitive.Root>
      {children != null && children !== false && (
        <span className="text-body-regular text-ink">{children}</span>
      )}
    </label>
  );
}

export { Switch };
