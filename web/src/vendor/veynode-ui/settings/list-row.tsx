import type { HTMLAttributes, ReactNode } from "react";
import type { LucideIcon } from "@veynode/ui/icons";
import { cx } from "../boardui/utils/cx";
import { useStateFit } from "./state-fit";

/** Plain divided rows; the default list surface, without a card border. */
export function List({
  className,
  children,
  ...rest
}: HTMLAttributes<HTMLDivElement>) {
  return (
    <div className={cx("divide-y divide-line", className)} {...rest}>
      {children}
    </div>
  );
}

/**
 * The line icon that leads a setting, an action or a way into a page: 20px in
 * the secondary ink, bare, as `SettingsRow icon` draws it. Give it to a
 * `SettingsLinkRow` that is not a thing in a list.
 */
export function RowIcon({ icon: Icon }: { icon: LucideIcon }) {
  return <Icon className="size-5 shrink-0 text-ink-2" aria-hidden />;
}

/**
 * The line icon that leads a thing in a list: 20px in the secondary ink, bare
 * (no plate), in the 32px width a thing's own mark (a computer's icon, a logo, an
 * avatar) takes, so the names of one list start on one edge; only as tall as
 * the icon, so the row keeps its 48px.
 */
export function ListRowIcon({ icon: Icon }: { icon: LucideIcon }) {
  return (
    <span className="flex w-8 shrink-0 justify-center text-ink-2" aria-hidden>
      <Icon className="size-5" />
    </span>
  );
}

/**
 * One row: leading icon, title with inline status, what it is (`meta`) at the
 * row's end on the same line, trailing actions and optional full-width
 * children. The row is one line at every width, a phone included: short of
 * room the meta is cut first, then the title; the status keeps its natural
 * width and is a dot only where even the title at its minimum does not fit
 * beside it (`useStateFit`); the actions never move under the row. Only a row whose content is a sentence (`metaLines={2}`) keeps
 * its meta under the title. With `onSelect` the content becomes
 * a button; actions stay outside it so nested controls remain valid.
 */
export function ListRow({
  icon,
  title,
  titleAs: Title = "span",
  titleAside,
  status,
  meta,
  metaLines = 1,
  metaWhole = false,
  actions,
  onSelect,
  selectDisabled,
  expanded,
  dimmed,
  as: Root = "div",
  className,
  children,
  ...rest
}: {
  icon?: ReactNode;
  title: ReactNode;
  titleAs?: "span" | "h2" | "h3";
  /**
   * Beside the title and outside it, such as the ⓘ (`InfoTip`) that holds
   * what the row is: the title stays the heading's whole name.
   */
  titleAside?: ReactNode;
  status?: ReactNode;
  meta?: ReactNode;
  /** Lines the meta may take before it is cut: two for a sentence, such as a member's role. */
  metaLines?: 1 | 2;
  /**
   * The meta is short and is what the row is read for (a time), so the title
   * is cut before it; otherwise the meta gives way at half the row.
   */
  metaWhole?: boolean;
  actions?: ReactNode;
  onSelect?: () => void;
  selectDisabled?: boolean;
  /** With `onSelect`, the row opens and closes what is under it. */
  expanded?: boolean;
  dimmed?: boolean;
  as?: "div" | "article" | "li";
  className?: string;
  children?: ReactNode;
} & Omit<HTMLAttributes<HTMLElement>, "title">) {
  const root = useStateFit();
  const content = (
    <>
      {icon}
      <div className={cx("min-w-0 flex-auto", dimmed && "opacity-60")}>
        <div className="flex min-w-0 items-center gap-x-2">
          {titleAside ? (
            <span className="flex min-w-0 items-center gap-1">
              <Title
                data-row-name=""
                className="block truncate text-body-medium text-ink"
              >
                {title}
              </Title>
              {titleAside}
            </span>
          ) : (
            <Title
              data-row-name=""
              className="block truncate text-body-medium text-ink"
            >
              {title}
            </Title>
          )}
          {status && (
            // A tag is taller than the title's line: it sits centred in that
            // line, so a row is as tall with a state as without one.
            // The title is cut before the state is.
            <span
              data-row-status=""
              className="group/state inline-flex h-5 shrink-0 items-center"
            >
              {status}
            </span>
          )}
        </div>
        {meta && metaLines === 2 && (
          <span className="mt-0.5 line-clamp-2 text-body-2-regular text-ink-2 [overflow-wrap:anywhere]">
            {meta}
          </span>
        )}
      </div>
      {meta && metaLines === 1 && (
        <span
          data-row-meta=""
          {...(metaWhole ? {} : { "data-row-squeeze": "" })}
          className={cx(
            "data-[squeezed]:hidden",
            metaWhole
              ? "shrink-0 text-body-2-regular text-ink-2"
              : "max-w-[50%] min-w-0 shrink-[100000] truncate text-body-2-regular text-ink-2",
            dimmed && "opacity-60",
          )}
        >
          {meta}
        </span>
      )}
    </>
  );
  return (
    <Root
      ref={root}
      {...(meta && metaLines === 2 ? { "data-two-line": "" } : {})}
      className={cx(
        "flex min-h-12 min-w-0 flex-wrap items-center gap-3 py-1.5 has-[[data-list-row-select]]:py-1",
        // In a settings group the whole row is lit, as a link row is, not
        // a box inside the group's box (SettingsGroup flattens the button).
        onSelect &&
          "transition-colors duration-[var(--motion-fast)] in-data-settings-group:has-[[data-list-row-select]:enabled:hover]:bg-ink/5",
        className,
      )}
      {...rest}
    >
      {onSelect ? (
        <button
          type="button"
          onClick={(event) => {
            event.currentTarget.focus();
            onSelect();
          }}
          disabled={selectDisabled}
          aria-expanded={expanded}
          className="-mx-1.5 flex min-w-0 flex-1 items-center gap-3 rounded-md px-2 py-1 text-left text-ink transition-colors duration-[var(--motion-fast)] outline-none hover:bg-ink/5 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus disabled:cursor-default disabled:hover:bg-transparent"
          data-list-row-select=""
        >
          {content}
        </button>
      ) : (
        <div
          data-list-row-body=""
          className="flex min-w-0 flex-1 items-center gap-3 px-1"
        >
          {content}
        </div>
      )}
      {actions && (
        <div className="ml-auto flex shrink-0 items-center gap-2">
          {actions}
        </div>
      )}
      {children && <div className="basis-full">{children}</div>}
    </Root>
  );
}
