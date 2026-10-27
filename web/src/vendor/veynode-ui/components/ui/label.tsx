import type { ComponentProps, ReactNode } from "react";
import { Info } from "../../icons";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Label (see packages/ui/shadcn-sources.json), drawn in Veynode
 * Soft: a field's label is small and strong (13/18, 600, ink), 4px from a
 * required asterisk (failed ink) and an info mark. An explanation belongs
 * behind an InfoTip beside the label, never in a sentence under it.
 *
 * Pass `required` to show the asterisk. `tooltip` draws a static info mark;
 * an explanation a person can open goes in `InfoTip`
 * (`@veynode/ui/settings/info-tip`) next to the label instead.
 */

/**
 * The one label over a block, whether the block is a field's control or a
 * group of rows: 13px semibold in ink, flush with the block's left edge.
 */
export const BLOCK_LABEL = "text-body-2-semibold text-ink";

export type LabelProps = ComponentProps<"label"> & {
  children: ReactNode;
  /** Shows the asterisk. */
  required?: boolean;
  /** Show the info icon next to the label. */
  tooltip?: boolean | string | undefined;
};

function Label({
  required = false,
  tooltip,
  className,
  children,
  ...props
}: LabelProps) {
  return (
    <label
      data-slot="label"
      data-label="true"
      className={cn(
        "flex cursor-default items-center gap-1",
        BLOCK_LABEL,
        className,
      )}
      {...props}
    >
      {children}
      {required && (
        <span
          aria-hidden="true"
          className="text-body-2-semibold text-state-failed-ink"
        >
          *
        </span>
      )}
      {tooltip && <Info className="size-4 shrink-0 text-ink-3" aria-hidden />}
    </label>
  );
}

export { Label };
