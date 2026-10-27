import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Badge (see packages/ui/shadcn-sources.json), drawn in Veynode
 * Soft: the one pill tag. 20px tall, 8px at the sides, caption medium, on a
 * tint of the text colour so it shows on a surface and on a group's fill
 * alike. It names a kind, a count or a note, never a state (states use
 * StateTag), so it carries no colour: `default` has its words in ink-2,
 * `strong` in ink.
 */
const badgeVariants = cva(
  "inline-flex h-5 items-center justify-center gap-1 rounded-full bg-ink/6 px-2 text-caption-1-medium whitespace-nowrap",
  {
    variants: {
      variant: { default: "text-ink-2", strong: "text-ink" },
    },
    defaultVariants: { variant: "default" },
  },
);

function Badge({
  className,
  variant,
  ...props
}: ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return (
    <span
      data-slot="badge"
      className={cn(badgeVariants({ variant }), className)}
      {...props}
    />
  );
}

export { Badge, badgeVariants };
