import type { HTMLAttributes, ReactNode } from "react";
import { BLOCK_LABEL } from "../components/ui/label";
import { cx } from "../boardui/utils/cx";

export { BLOCK_LABEL };

/**
 * One titled group inside a settings panel: its label over its content,
 * written as a field's label is (`BLOCK_LABEL`) and flush with the content's
 * left edge, 6px above it; with `actions` the label's row is as tall as
 * they are. Set apart by space rather than a rule, and
 * what the group does (`description`) said once under it, never over it; the
 * parent separates sibling sections with `space-y-6` (compact) or
 * `space-y-8`. Rows usually sit in a `SettingsGroup`.
 */
export function SettingsSection({
  title,
  description,
  actions,
  headingLevel = 2,
  className,
  children,
  ...rest
}: {
  title?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  headingLevel?: 2 | 3;
  className?: string;
  children: ReactNode;
} & Omit<HTMLAttributes<HTMLElement>, "title">) {
  const Heading = headingLevel === 3 ? "h3" : "h2";
  return (
    <section className={cx("space-y-1.5", className)} {...rest}>
      {(title || actions) && (
        <div
          className={cx(
            "flex flex-wrap items-center justify-between gap-x-4 gap-y-1",
            actions && "min-h-7",
          )}
        >
          <div className="min-w-0 flex-1 space-y-0.5">
            {title && <Heading className={BLOCK_LABEL}>{title}</Heading>}
          </div>
          {actions && (
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              {actions}
            </div>
          )}
        </div>
      )}
      {children}
      {description && <SettingsFootnote>{description}</SettingsFootnote>}
    </section>
  );
}

/**
 * The group's box on its own, for a list that places its rows itself (a
 * virtual list), which then insets and divides them as the group would. A
 * quiet fill and no edge (the veil of design-system/foundations.md,
 * "Materials"): on a see-through pane it is a tint of the text colour, where
 * a white box with a hairline would not show.
 */
export const SETTINGS_GROUP_BOX = "overflow-hidden rounded-lg bg-surface-2";

/** The words under a group: what its rows do, said once (small, ink-2). */
export function SettingsFootnote({
  className,
  children,
  ...rest
}: {
  className?: string;
  children: ReactNode;
} & HTMLAttributes<HTMLParagraphElement>) {
  return (
    <p
      data-settings-footnote=""
      className={cx("text-body-2-regular text-ink-2", className)}
      {...rest}
    >
      {children}
    </p>
  );
}

/**
 * Settings rows in one box (the veil's fill, lg radius, no edge, dividers
 * between rows that stop 16px short of each side), the Veynode Soft settings
 * group. It pads the rows inside it; put `SettingsRow`s, `SettingsLinkRow`s, `ListRow`s or plain
 * rows straight in it. A `ListRow` that opens is lit as a whole row, as a
 * `SettingsLinkRow` is, never as a box inside the group's box. `footnote` is said under the box: the explanation a
 * page would otherwise put above its rows.
 */
export function SettingsGroup({
  className,
  footnote,
  children,
  ...rest
}: {
  className?: string;
  footnote?: ReactNode;
  children: ReactNode;
} & HTMLAttributes<HTMLDivElement>) {
  const box = (
    <div
      data-settings-group=""
      className={cx(
        SETTINGS_GROUP_BOX,
        "[&>*]:relative [&>*]:border-0! [&>*]:px-4! [&>*]:py-1.5! [&>[data-two-line]]:py-2! [&_[data-list-row-body]]:px-0! [&_[data-list-row-select]]:mx-0! [&_[data-list-row-select]]:px-0! [&_[data-list-row-select]]:py-0! [&_[data-list-row-select]]:bg-transparent! [&>*+*]:before:pointer-events-none [&>*+*]:before:absolute [&>*+*]:before:inset-x-4 [&>*+*]:before:top-0 [&>*+*]:before:h-px [&>*+*]:before:bg-line",
        className,
      )}
      {...rest}
    >
      {children}
    </div>
  );
  if (!footnote) return box;
  return (
    <div className="space-y-2">
      {box}
      <SettingsFootnote>{footnote}</SettingsFootnote>
    </div>
  );
}
