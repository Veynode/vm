import type { HTMLAttributes, MouseEvent, ReactNode } from "react";
import { ChevronRight } from "@veynode/ui/icons";
import { cx } from "../boardui/utils/cx";
import { useStateFit } from "./state-fit";

/**
 * A right click on a row opens its ··· menu (`RowMenu`), where it has one;
 * a row without one keeps the browser's own menu.
 */
export function openRowMenu(event: MouseEvent<HTMLElement>) {
  const trigger = event.currentTarget.querySelector<HTMLButtonElement>(
    "[data-row-menu] button:not(:disabled)",
  );
  if (!trigger) return;
  event.preventDefault();
  trigger.click();
}

/**
 * The one row that opens a sub-page of Settings (design-system/settings.md,
 * "Navigation"): an optional leading icon, the label, a state, then at the
 * end what it is (`meta`), what it is set to (`value`) and a chevron (none
 * where the row ends in a ··· menu), all on
 * one line of a fixed 48px at every width (Veynode Soft Layout 13: a row has
 * no second line, save the one `line` that only the memory keeper's row
 * takes): a long name is cut, never wrapped under. Short of room
 * the `meta` and the `value` give way first, then the name; the state keeps
 * its natural width and becomes a dot only where even the name at its
 * minimum does not fit beside it (`useStateFit`). A press
 * anywhere on the row opens it: the label is the button, stretched over the
 * whole row, so a status tag with a tip and the row's own buttons (`status`,
 * `actions`) stay pressable above it: they come after it and are positioned,
 * so they paint over its stretched area without a z-index. A button wrapping
 * the whole row could not hold them: buttons do not nest. A right click on
 * the row opens the ··· menu among its `actions`, where it has one.
 *
 * Put these straight inside a `SettingsGroup`, which pads and divides them.
 */
export function SettingsLinkRow({
  icon,
  label,
  labelAs: Label = "span",
  labelAside,
  status,
  meta,
  line,
  value,
  actions,
  onOpen,
  openLabel,
  disabled = false,
  dimmed = false,
  chevron = true,
  wrap = false,
  as: Root = "div",
  className,
  children,
  ...rest
}: {
  /** Before the label: a bare 20px line icon, or a thing's own 32px mark. */
  icon?: ReactNode;
  /** The row's name; it opens the row. */
  label: ReactNode;
  /** `h3` where the row names a thing a page lists (a node, a connection). */
  labelAs?: "span" | "h3";
  /** Beside the label and outside its button, such as an ⓘ (`InfoTip`). */
  labelAside?: ReactNode;
  /** A state tag on the label's line. */
  status?: ReactNode;
  /** What the thing is, in a few words, at the row's end; cut when long. */
  meta?: ReactNode;
  /**
   * A second line under the label, cut at its end: what the row is set to
   * when that is a phrase too long to share the label's line (the memory
   * keeper's model and times).
   */
  line?: ReactNode;
  /** What the sub-page is set to now, kept short, before the chevron. */
  value?: ReactNode;
  /** The row's own controls, after the value: its main action, a ··· menu. */
  actions?: ReactNode;
  onOpen: () => void;
  /** The opening button's name when the label alone would not say where it goes. */
  openLabel?: string | undefined;
  disabled?: boolean | undefined;
  dimmed?: boolean | undefined;
  /** Off where the row ends in its own action (Install), which says enough. */
  chevron?: boolean | undefined;
  /**
   * The label is read whole, up to three lines, and what ends the row
   * stands at its first line: a row that asks about its own words (a memory
   * waiting for review). Its children still go under it.
   */
  wrap?: boolean | undefined;
  as?: "div" | "article" | "li";
  className?: string;
  /** Full-width lines under the row (what it needs from its person). */
  children?: ReactNode;
} & Omit<HTMLAttributes<HTMLElement>, "title">) {
  const root = useStateFit();
  const metaNode = meta && (
    <span
      data-row-meta=""
      className={cx(
        "min-w-0 truncate text-body-2-regular text-ink-2",
        dimmed && "opacity-60",
      )}
    >
      {meta}
    </span>
  );
  const valueNode = value !== undefined &&
    value !== null &&
    value !== false && (
      <span className="max-w-60 min-w-0 truncate text-body-2-regular text-ink-2 tabular-nums">
        {value}
      </span>
    );
  const actionsNode = actions && (
    <span className="relative flex shrink-0 items-center justify-end gap-2">
      {actions}
    </span>
  );
  const chevronNode = chevron && (
    <ChevronRight
      aria-hidden
      className={cx(
        // A row that ends in its ··· menu draws no chevron beside it.
        "size-4 shrink-0 text-ink-3 group-has-[[data-row-menu]]/row:hidden rtl:rotate-180",
        disabled && "opacity-50",
      )}
    />
  );
  return (
    <Root
      ref={root}
      data-settings-link-row=""
      className={cx(
        "group/row relative flex min-w-0 items-center gap-x-3 transition-colors duration-[var(--motion-fast)] has-[[data-open-row]:hover:not(:disabled)]:bg-ink/5",
        // Lines under the row and a wrapped label are what make it taller.
        wrap
          ? "min-h-12 flex-wrap items-start gap-y-1 py-1.5"
          : children
            ? "min-h-12 flex-wrap gap-y-1 py-1.5"
            : line
              ? "min-h-12 py-2"
              : "h-12",
        className,
      )}
      onContextMenu={openRowMenu}
      {...rest}
    >
      {icon}
      <div
        className={cx(
          "min-w-0",
          // As wide as its content (the end of the row gives way first,
          // `shrink-[100000]` below), taking what is left over.
          wrap ? "flex-1" : "flex-auto",
          dimmed && "opacity-60",
          wrap && "py-2",
        )}
      >
        <div className="flex min-w-0 items-center gap-x-2">
          <span className="flex min-w-0 items-center gap-1">
            <Label
              className={cx(
                "block min-w-0 text-body-medium text-ink",
                !wrap && "truncate",
              )}
            >
              <button
                type="button"
                data-open-row=""
                aria-label={openLabel}
                disabled={disabled}
                onClick={(event) => {
                  event.currentTarget.focus();
                  onOpen();
                }}
                className="flex max-w-full min-w-0 cursor-pointer text-left outline-none after:absolute after:inset-0 after:content-[''] focus-visible:after:outline-2 focus-visible:after:-outline-offset-2 focus-visible:after:outline-focus disabled:cursor-default"
              >
                {/* A button centres content wider than itself; the name is
                    cut at its end instead. */}
                <span
                  data-row-name=""
                  className={cx(
                    "min-w-0",
                    wrap ? "line-clamp-3 [overflow-wrap:anywhere]" : "truncate",
                  )}
                >
                  {label}
                </span>
              </button>
            </Label>
            {labelAside && <span className="relative flex">{labelAside}</span>}
          </span>
          {status && (
            // The state keeps its natural width: the name is cut before it
            // is, and it is a dot only where the name would be left under
            // its minimum (`useStateFit`). Words that only describe
            // (`data-passive`) let a press through to the row. As dots it
            // keeps one: a tag that asks something hides the role's
            // (`data-row-role`).
            <span
              data-row-status=""
              className="group/state relative inline-flex shrink-0 items-center gap-1 has-[[data-passive]]:pointer-events-none data-[compact]:has-[[data-state-badge=waiting]]:[&_[data-row-role]]:hidden data-[compact]:has-[[data-state-badge=failed]]:[&_[data-row-role]]:hidden"
            >
              {status}
            </span>
          )}
        </div>
        {line && (
          <span
            data-row-line=""
            className="block truncate text-body-2-regular text-ink-2"
          >
            {line}
          </span>
        )}
      </div>
      {wrap ? (
        <span className="mt-1 ml-auto flex max-w-[60%] min-w-0 shrink-0 items-center gap-3">
          {metaNode}
          {valueNode}
          {actionsNode}
          {chevronNode}
        </span>
      ) : (
        <>
          {(metaNode || valueNode) && (
            // What is cut first: the name and the state keep their room and
            // this shrinks to nothing before they give any. The weight is
            // large enough that the name loses no fraction of a pixel,
            // which WebKit draws as a letter lost to the ellipsis.
            <span
              data-row-squeeze=""
              className="flex min-w-0 shrink-[100000] items-center gap-3 data-[squeezed]:hidden"
            >
              {metaNode}
              {valueNode}
            </span>
          )}
          {(actionsNode || chevronNode) && (
            <span className="flex shrink-0 items-center gap-3">
              {actionsNode}
              {chevronNode}
            </span>
          )}
        </>
      )}
      {/* A caller's lines may render nothing; an empty wrapper would still
          take a wrapped line and its gap under the row. */}
      {children && (
        <div
          className={cx(
            // Words that only describe (`data-passive`) let a press through
            // to the row, as its status does.
            "relative basis-full empty:hidden has-[[data-passive]]:pointer-events-none",
            wrap && "pb-2",
          )}
        >
          {children}
        </div>
      )}
    </Root>
  );
}
