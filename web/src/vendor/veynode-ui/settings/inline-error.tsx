import type { HTMLAttributes, ReactNode } from "react";
import { TriangleAlert } from "@veynode/ui/icons";
import { cx } from "../boardui/utils/cx";

/**
 * Validation or action error directly under the control it belongs to: a
 * small alert mark and one short sentence in the failed ink, so colour is not
 * the only cue (ds `.vs-error`). The mark sits in the line, so the text
 * alignment a caller sets still applies. A panel that did not load uses
 * `PanelLoadError`; raw error text goes in a DetailTip, not here.
 */
export function InlineError({
  children,
  id,
  className,
  ...rest
}: {
  children: ReactNode;
  id?: string;
  className?: string;
} & Omit<HTMLAttributes<HTMLParagraphElement>, "children" | "className" | "id">) {
  return (
    <p
      role="alert"
      id={id}
      {...rest}
      className={cx(
        "text-caption-1-medium text-state-failed-ink [overflow-wrap:anywhere]",
        className,
      )}
    >
      <TriangleAlert
        className="me-1 inline size-3.5 -translate-y-px align-middle"
        aria-hidden
      />
      {children}
    </p>
  );
}
