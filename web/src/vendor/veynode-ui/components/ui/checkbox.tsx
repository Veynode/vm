import { Checkbox as CheckboxPrimitive } from "@base-ui/react/checkbox";
import type { ReactNode } from "react";
import { Check, Minus } from "../../icons";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Checkbox (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: an 18px box (16px at `sm`) with a 1.5px line-strong
 * edge; checked or indeterminate it fills with ink and the mark is
 * on-action. With `children` the box and its words are one label.
 */
type CheckboxSize = "sm" | "md";

const sizes: Record<
  CheckboxSize,
  { box: string; glyph: string; label: string; gap: string }
> = {
  md: {
    box: "size-[18px]",
    glyph: "size-3.5",
    label: "text-body-regular",
    gap: "gap-2",
  },
  sm: {
    box: "size-4",
    glyph: "size-3",
    label: "text-body-2-regular",
    gap: "gap-1.5",
  },
};

export type CheckboxProps = Omit<
  CheckboxPrimitive.Root.Props,
  "children" | "className"
> & {
  /** The words beside the box. */
  children?: ReactNode;
  size?: CheckboxSize;
  /** On the label when there are words, else on the box. */
  className?: string | undefined;
};

function Box({ size, className, ...props }: Omit<CheckboxProps, "children">) {
  const s = sizes[size ?? "md"];
  return (
    <CheckboxPrimitive.Root
      data-slot="checkbox"
      className={cn(
        "relative flex shrink-0 items-center justify-center rounded-xs",
        "bg-surface text-on-action inset-ring-[1.5px] inset-ring-line-strong hover:inset-ring-ink-2",
        "transition-[background-color,box-shadow] duration-[var(--button-transition-ms)] ease-standard",
        "data-checked:bg-action data-checked:inset-ring-0 data-indeterminate:bg-action data-indeterminate:inset-ring-0",
        "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        "data-disabled:cursor-not-allowed data-disabled:opacity-45 data-disabled:hover:inset-ring-line-strong",
        s.box,
        className,
      )}
      {...props}
    >
      <CheckboxPrimitive.Indicator
        data-slot="checkbox-indicator"
        className="flex items-center justify-center"
      >
        {props.indeterminate ? (
          <Minus className={cn("shrink-0", s.glyph)} aria-hidden />
        ) : (
          <Check className={cn("shrink-0", s.glyph)} aria-hidden />
        )}
      </CheckboxPrimitive.Indicator>
    </CheckboxPrimitive.Root>
  );
}

function Checkbox({
  children,
  size = "md",
  className,
  ...props
}: CheckboxProps) {
  if (children === undefined || children === null || children === false)
    return <Box size={size} className={className} {...props} />;
  const s = sizes[size];
  return (
    <label
      className={cn(
        "inline-flex items-center select-none",
        props.disabled ? "cursor-not-allowed" : "cursor-pointer",
        s.gap,
        className,
      )}
    >
      <Box size={size} {...props} />
      <span className={cn(s.label, "text-ink")}>{children}</span>
    </label>
  );
}

export { Checkbox };
