import type { ReactNode } from "react";
import { cx } from "../boardui/utils/cx";
import { Logo } from "../brand/logo";

/**
 * The row every column's head stands on: 56px, never taller, so the titles
 * across the window share one line (design-system/shell.md, "Heads"). The
 * sidebars' head, a side editor's and the floating details pane's close row
 * are this row with their own insets.
 */
export function PaneHead({
  as: Tag = "header",
  className,
  children,
}: {
  as?: "header" | "div";
  className?: string;
  children?: ReactNode;
}) {
  return (
    <Tag
      className={cx(
        "flex h-14 min-h-14 shrink-0 items-center gap-2",
        className,
      )}
    >
      {children}
    </Tag>
  );
}

/**
 * The head of a sidebar (the chats, Settings, Space): one 56px row, the
 * product's mark at the start and its buttons at the end. Every sidebar's
 * head is this one, so the mark stands at the same place and on the same line
 * as the page header beside it. The button's glyph stands as far from the end
 * as the mark from the start (20px). The place's name is a heading for
 * assistive technology only: the places control under the head shows which
 * place it is. In the Mac app the row is the title bar: the window's lights
 * sit at its start and the mark after them (`.sidebar-head` in the console's
 * chrome-glass-desktop.css). In the Windows app the mark stands in the title
 * bar above instead (`.desktop-window-header`, chrome-glass.css).
 */
export function SidebarHead({
  title,
  children,
  className,
}: {
  title: string;
  /** Its buttons, at the end of the row. */
  children?: ReactNode;
  className?: string;
}) {
  return (
    <PaneHead className={cx("sidebar-head pr-3 pl-5", className)}>
      {/* The mark stands where a title would; the title stays for assistive technology and the window's heading. */}
      <div className="flex min-w-0 flex-1 items-center">
        <Logo size={28} wordmark className="sidebar-head-mark" />
        <h1 className="sr-only">{title}</h1>
      </div>
      {children}
    </PaneHead>
  );
}

/**
 * The scrolling body of a sidebar of navigation rows (Settings' pages,
 * Space's places): rows 2px apart on a 12px inset (20px on a phone, the
 * head's mark), the last 24px clear of the foot (80px on a phone, where the
 * places float over the list). With a `label`
 * it is a `nav` of that name. The chats' list is its own scroller
 * (`VirtualConversationList`): it measures and positions rows.
 */
export function SidebarList({
  label,
  className,
  children,
}: {
  label?: string;
  className?: string;
  children: ReactNode;
}) {
  const classes = cx(
    "sidebar-list flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto overscroll-contain px-3 pb-16 pt-1 max-narrow:px-5 max-narrow:pb-20 [-webkit-app-region:no-drag]",
    className,
  );
  return label ? (
    <nav aria-label={label} className={classes}>
      {children}
    </nav>
  ) : (
    <div className={classes}>{children}</div>
  );
}

/**
 * The rows of a `SidebarList`. On a phone, where the list is white, they
 * stand on one grey rounded block with a hairline between rows, as an iOS
 * grouped list (base.css, `sidebar-row`); a row's fill on a press is a step
 * darker there (`SettingsNavRow`).
 */
export function SidebarGroup({ children }: { children: ReactNode }) {
  return (
    <div className="sidebar-group flex flex-col gap-0.5 max-narrow:gap-0 max-narrow:rounded-xl max-narrow:bg-surface-2 max-narrow:p-1">
      {children}
    </div>
  );
}

/**
 * The foot of a sidebar, under its list, on the same 12px inset: a window's
 * own close. (The app's places are `SidebarTabs`, under the head.)
 */
export function SidebarFoot({ children }: { children: ReactNode }) {
  return (
    <div className="sidebar-foot shrink-0 px-3 [-webkit-app-region:no-drag]">
      {children}
    </div>
  );
}
