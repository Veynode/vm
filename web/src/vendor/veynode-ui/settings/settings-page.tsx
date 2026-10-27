import {
  createContext,
  useEffect,
  useId,
  useRef,
  useState,
  type ComponentType,
  type ReactNode,
} from "react";
import { ArrowLeft, ChevronRight } from "@veynode/ui/icons";
import { IconButton } from "../components/ui/icon-button";
import { Badge } from "../components/ui/badge";
import { cx } from "../boardui/utils/cx";
import { useSwipeBack } from "../boardui/overlays/swipe-back";
import {
  SidebarFoot,
  SidebarGroup,
  SidebarHead,
  SidebarList,
} from "./sidebar-head";

/**
 * Where a page puts its own actions: the title band, at its end, so a page
 * opens straight onto its title, its buttons and its groups. Null outside
 * Settings, where a page draws its actions itself (`PanelHeader`).
 */
export const SettingsActionsSlot = createContext<HTMLElement | null>(null);

type TileIcon = ComponentType<{ className?: string; "aria-hidden"?: boolean }>;

/**
 * One row of the settings list: the page's icon (18px, bare, in the name's
 * colour), its name and what it is set to, 36px tall. A name is regular in
 * `ink-2`, the shown page's medium in `ink`, so the one shown stands out by
 * weight and light as well as by its fill. On a phone it is 44px, the icon
 * 20px in a 40px slot, and every name is `ink`. The rows stand on the
 * floor like the chats beside them: hover is a
 * quiet fill and the page being shown takes the surface's, the way the chat
 * list marks its current chat. The hover fill eases; the shown page's changes
 * at once, with the page: on light glass hover darkens the floor and the
 * surface lightens it, so a blend from one to the other passes through no
 * fill at all and the pressed row blinks. On a phone, where the list and the
 * page take turns, nothing is marked and a chevron says the row opens a page.
 */
export function SettingsNavRow({
  icon: Icon,
  label,
  value,
  attention,
  selected = false,
  disabled = false,
  onSelect,
}: {
  icon: TileIcon;
  label: ReactNode;
  /** What the page is set to now, kept short. */
  value?: ReactNode;
  /**
   * Something on the page waits for the person: a count draws a number,
   * otherwise a dot; `label` says it in words, so colour is never the only sign.
   */
  attention?: { label: string; count?: number };
  selected?: boolean;
  /**
   * The page cannot be opened now; `value` says why. The row stays in the
   * list, greyed, and still reads out for a screen reader.
   */
  disabled?: boolean;
  onSelect: () => void;
}) {
  const note = useId();
  return (
    <button
      type="button"
      aria-current={selected ? "page" : undefined}
      aria-describedby={attention ? note : undefined}
      aria-disabled={disabled || undefined}
      onClick={disabled ? undefined : onSelect}
      className="sidebar-row group/row flex min-h-9 w-full min-w-0 cursor-pointer items-center gap-3 rounded-sm px-2.5 text-left max-narrow:rounded-md max-narrow:min-h-11 max-narrow:px-2 outline-none transition-colors duration-[var(--motion-fast)] hover:bg-surface-2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus aria-[current=page]:bg-surface aria-[current=page]:transition-none max-narrow:hover:bg-field max-narrow:aria-[current=page]:bg-transparent max-narrow:aria-[current=page]:hover:bg-field aria-disabled:cursor-default aria-disabled:hover:bg-transparent"
    >
      <span
        aria-hidden
        className="flex w-4.5 shrink-0 justify-center text-ink-2 max-narrow:w-10 group-hover/row:text-ink group-aria-[current=page]/row:text-ink max-narrow:group-hover/row:text-ink-2 max-narrow:group-aria-[current=page]/row:text-ink-2 group-aria-disabled/row:text-ink-3"
      >
        <Icon className="size-4.5 max-narrow:size-5" aria-hidden />
      </span>
      <span className="min-w-0 flex-1 truncate text-body-regular text-ink-2 group-hover/row:text-ink group-aria-[current=page]/row:text-body-medium group-aria-[current=page]/row:text-ink max-narrow:text-body-medium max-narrow:text-ink group-aria-disabled/row:text-ink-3">
        {label}
      </span>
      {value && (
        <span
          className={cx(
            "truncate text-right text-body-2-regular text-ink-2",
            // A row that cannot open is greyed and nothing more (the capsule
            // says why); the reason is only read out.
            disabled ? "sr-only" : "ml-auto max-w-[40%] shrink-0",
          )}
        >
          {value}
        </span>
      )}
      {attention &&
        (attention.count ? (
          <Badge aria-hidden="true" className="shrink-0 py-0">
            {attention.count}
          </Badge>
        ) : (
          <span
            aria-hidden="true"
            className="size-2 shrink-0 rounded-full bg-ink"
          />
        ))}
      {attention && (
        <span id={note} hidden>
          {attention.label}
        </span>
      )}
      <ChevronRight
        className="hidden size-4 shrink-0 text-ink-3 max-narrow:block max-narrow:group-aria-disabled/row:hidden rtl:rotate-180"
        aria-hidden
      />
    </button>
  );
}

/**
 * The person at the top of the settings list: their picture, name and a line
 * under it, opening their profile page. The list's first row, marked like
 * the rows under it.
 */
export function ProfileCard({
  avatar,
  name,
  detail,
  selected = false,
  onSelect,
}: {
  avatar: ReactNode;
  name: ReactNode;
  detail?: ReactNode;
  selected?: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      aria-current={selected ? "page" : undefined}
      onClick={onSelect}
      className="flex min-h-14 w-full min-w-0 cursor-pointer items-center gap-3 rounded-md px-2 py-2 text-left outline-none transition-colors duration-[var(--motion-fast)] hover:bg-surface-2 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus aria-[current=page]:bg-surface aria-[current=page]:transition-none max-narrow:hover:bg-field max-narrow:aria-[current=page]:bg-transparent max-narrow:aria-[current=page]:hover:bg-field"
    >
      {avatar}
      <span className="min-w-0 flex-1">
        <span className="block truncate text-body-semibold text-ink">
          {name}
        </span>
        {detail && (
          <span className="mt-0.5 block truncate text-body-2-regular text-ink-2">
            {detail}
          </span>
        )}
      </span>
      <ChevronRight
        className="hidden size-4 shrink-0 text-ink-3 max-narrow:block rtl:rotate-180"
        aria-hidden
      />
    </button>
  );
}

export type SettingsPageItem = {
  id: string;
  label: string;
  icon: TileIcon;
  value?: ReactNode;
  attention?: { label: string; count?: number };
  /** Greyed and not opened; `value` carries the reason. */
  disabled?: boolean;
};
export type SettingsPageGroup = {
  id: string;
  /** A small grey heading over the group; leave it out for a group of its own. */
  label?: string;
  items: SettingsPageItem[];
};

/**
 * Whether a dialog, menu or list opened over Settings owns the keyboard right
 * now. What comes after Settings in the document counts, and so does a dialog
 * a page of Settings draws inside it (a sheet, a confirmation): one Escape
 * closes that and nothing else. A dialog left open underneath (a Bot editor
 * Settings was opened from) never keeps Escape or Tab from Settings.
 */
function overlayAbove(root: HTMLElement | null) {
  return Array.from(
    document.querySelectorAll<HTMLElement>(
      '[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"], .overlay',
    ),
  ).some(
    (node) =>
      node.getClientRects().length > 0 &&
      (!root ||
        (root.contains(node)
          ? node !== root &&
            node.matches('[role="dialog"], [role="alertdialog"], .overlay')
          : (root.compareDocumentPosition(node) &
              Node.DOCUMENT_POSITION_FOLLOWING) !==
            0)),
  );
}

/** The controls Tab can reach inside Settings, in reading order. */
function reachable(root: HTMLElement | null) {
  return Array.from(
    root?.querySelectorAll<HTMLElement>(
      'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]',
    ) ?? [],
  ).filter(
    (node) =>
      node.getClientRects().length > 0 &&
      node.tabIndex >= 0 &&
      !node.closest('[aria-hidden="true"]'),
  );
}

/**
 * One end of the title band: nothing at rest, a floating capsule once the
 * page has scrolled under the band (`group/band` carries `data-scrolled`).
 * 40px tall, the conversation header's capsule; 44px on a phone, where the
 * controls inside are 44px. Space's band is the same.
 */
export const SETTINGS_CAPSULE =
  "pointer-events-auto flex h-10 min-w-0 items-center gap-2 rounded-full max-narrow:h-11 transition-[background-color,box-shadow] duration-200 group-data-[scrolled]/band:bg-surface group-data-[scrolled]/band:shadow-float";

/** Below the `narrow` breakpoint (600px) the list and the page take turns. */
const NARROW = "(max-width: 599.98px)";

/** What the page list is drawn in: its classes, its name and which of list and page shows. */
export type SettingsListFrameProps = {
  className: string;
  label: string;
  view: "list" | "page";
  children: ReactNode;
};

function NavFrame({
  className,
  label,
  view,
  children,
}: SettingsListFrameProps) {
  return (
    <nav aria-label={label} data-view={view} className={className}>
      {children}
    </nav>
  );
}

/**
 * Settings as a page of its own, filling the window: the list of pages on
 * the floor (the pages as one run of rows),
 * and the selected page as a surface pane beside it. The page's title rests
 * on the pane and turns into a floating capsule once the page scrolls under
 * it. Below the narrow breakpoint they take turns:
 * `view` says which one shows, the page's back button returns to the list
 * through `onList`. `places` under the head lead back to the rest of the app
 * and `footer` under the list is a window's own close; Escape leaves
 * Settings (`onClose`).
 * A page puts its actions into the title row through `SettingsActionsSlot`.
 */
export function SettingsPage({
  title,
  label,
  backLabel,
  places,
  footer,
  profile,
  groups,
  page,
  row,
  view,
  onPageChange,
  onList,
  onClose,
  heading,
  back,
  backDisabled = false,
  standalone = false,
  containFocus = false,
  listFrame,
  children,
}: {
  /** The list's title. */
  title: string;
  /** The page region's name for a screen reader. */
  label: string;
  /** Names the page's back button. */
  backLabel: string;
  /** Under the head: the way to the rest of the app (the chats), when there is one. */
  places?: ReactNode;
  /** Under the list: a window's own close, where there is one. */
  footer?: ReactNode;
  /** A profile card, above the groups, where the list has one. */
  profile?: ReactNode;
  groups: SettingsPageGroup[];
  /** The page shown (the highlighted row). */
  page: string;
  /**
   * The row to highlight when the page shown is not a row of its own (a page
   * under one, such as a node's Desktop under This machine); `page` otherwise.
   */
  row?: string | undefined;
  /** Narrow only: whether the list or the page is on screen. */
  view: "list" | "page";
  onPageChange: (page: string) => void;
  /** Narrow only: the page's back button, which shows the list again. */
  onList: () => void;
  /** Leaves Settings (Escape). A window that is only Settings has none. */
  onClose?: (() => void) | undefined;
  /** A page's own title when it is not its row's name (a sub-page). */
  heading?: ReactNode;
  /** A sub-page's way one level up. Escape takes it instead of leaving. */
  back?: (() => void) | undefined;
  backDisabled?: boolean | undefined;
  /**
   * Settings is a window of its own (the desktop app): the page list starts
   * below the window's controls and the window moves by what a title bar
   * would be.
   */
  standalone?: boolean;
  /**
   * Settings covers the rest of the window (the workspace's overlay): Tab
   * goes round inside it and never reaches what is under it, unless a
   * dialog, menu or list opened over it owns the keyboard.
   */
  containFocus?: boolean;
  /**
   * What holds the page list. By default a fixed-width `nav`; the workspace
   * passes the frame the chats sidebar uses, so the list is the same size
   * and is resized the same way, and the one width is shared.
   */
  listFrame?: ComponentType<SettingsListFrameProps> | undefined;
  children: ReactNode;
}) {
  const titleId = useId();
  const [actions, setActions] = useState<HTMLDivElement | null>(null);
  // Whether the page has scrolled under its title: only then is the title a
  // floating capsule. A new page starts at its top.
  const [scrolled, setScrolled] = useState(false);
  useEffect(() => setScrolled(false), [page]);
  const latest = useRef({ back, backDisabled, view, onClose, onList });
  latest.current = { back, backDisabled, view, onClose, onList };
  const root = useRef<HTMLDivElement>(null);
  // A phone's swipe from the edge does what a top page's Back does: the
  // list. A deeper page's Back leads to another page, not drawn behind it.
  const detail = useRef<HTMLDivElement>(null);
  useSwipeBack(detail, ".settings-list", view === "page" && !back, onList);
  useEffect(() => {
    root.current?.focus({ preventScroll: true });
  }, []);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || event.defaultPrevented || event.isComposing)
        return;
      if (overlayAbove(root.current)) return;
      const now = latest.current;
      if (now.back) {
        if (!now.backDisabled) now.back();
      } else if (now.view === "page" && window.matchMedia(NARROW).matches)
        now.onList();
      else now.onClose?.();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  // Over the rest of the window, Tab stays in Settings: from its last
  // control it goes round to the first, and whatever under it takes focus
  // (Safari's Tab skips buttons, so the wrap may never see the last one)
  // sends it back to the first control.
  useEffect(() => {
    if (!containFocus) return;
    const onTab = (event: KeyboardEvent) => {
      const node = root.current;
      if (event.key !== "Tab" || !node || overlayAbove(node)) return;
      const nodes = reachable(node);
      const first = nodes[0];
      const last = nodes.at(-1);
      const at = document.activeElement;
      if (event.shiftKey && (at === first || at === node)) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && (at === last || at === node)) {
        event.preventDefault();
        first?.focus();
      }
    };
    const onFocus = (event: FocusEvent) => {
      const node = root.current;
      const target = event.target;
      if (!node || !(target instanceof HTMLElement) || node.contains(target))
        return;
      // Only what is under Settings; toasts and native menus sit in the top
      // layer above it, and a dialog opened over it keeps its focus.
      if (
        (node.compareDocumentPosition(target) &
          Node.DOCUMENT_POSITION_PRECEDING) ===
          0 ||
        target.closest("[popover]") ||
        overlayAbove(node)
      )
        return;
      (reachable(node)[0] ?? node).focus();
    };
    window.addEventListener("keydown", onTab);
    document.addEventListener("focusin", onFocus);
    return () => {
      window.removeEventListener("keydown", onTab);
      document.removeEventListener("focusin", onFocus);
    };
  }, [containFocus]);
  const selected = groups
    .flatMap((group) => group.items)
    .find((item) => item.id === page);
  const ListFrame = listFrame ?? NavFrame;
  const upLabel = backLabel;
  return (
    <div
      ref={root}
      role="region"
      aria-label={label}
      tabIndex={-1}
      // Over the window, Settings shares the dialog layer: a dialog it was
      // opened over leaves Escape and Tab to it (`isTopDialog`).
      data-dialog-layer={containFocus ? "" : undefined}
      className={cx(
        "settings-page flex h-dvh min-h-0 gap-2 overflow-hidden py-2 pr-2 text-body-regular text-ink outline-none max-narrow:gap-0 max-narrow:p-0",
        standalone && "[-webkit-app-region:drag]",
      )}
    >
      <ListFrame
        label={title}
        view={view}
        className={cx(
          // The widths of the chats sidebar (chat-sidebar), so the two panes
          // are the same size in every layout. `settings-list` is the hook the
          // app's glass styles take, as they take `chat-sidebar`.
          "settings-list relative flex w-[300px] shrink-0 flex-col max-narrow:w-full max-narrow:data-[view=page]:hidden",
          standalone && "pt-[var(--window-controls-inset,0px)]",
        )}
      >
        <SidebarHead title={title} />
        {places}
        <SidebarList>
          {profile && <div className="mb-3">{profile}</div>}
          <SidebarGroup>
            {groups.map((group) => (
              <section
                key={group.id}
                className={cx("space-y-1.5", group.label && "not-first:mt-3")}
              >
                {group.label && (
                  <h2 className="px-2 text-caption-1-semibold text-ink-2 max-narrow:px-3">
                    {group.label}
                  </h2>
                )}
                <div className="flex flex-col gap-0.5 max-narrow:gap-0">
                  {group.items.map((item) => (
                    <SettingsNavRow
                      key={item.id}
                      icon={item.icon}
                      label={item.label}
                      value={item.value}
                      {...(item.attention ? { attention: item.attention } : {})}
                      selected={item.id === (row ?? page)}
                      disabled={item.disabled === true}
                      onSelect={() => onPageChange(item.id)}
                    />
                  ))}
                </div>
              </section>
            ))}
          </SidebarGroup>
        </SidebarList>
        {footer && <SidebarFoot>{footer}</SidebarFoot>}
      </ListFrame>
      <div
        ref={detail}
        data-view={view}
        className={cx(
          "relative flex min-h-0 min-w-0 flex-1 flex-col overflow-clip rounded-xl bg-surface max-narrow:rounded-none max-narrow:data-[view=list]:hidden",
          // `settings-detail` is the pane's hook for the Mac window's glass;
          // `settings-window-page` is the same in a window of its own.
          "settings-detail",
          standalone && "settings-window-page [-webkit-app-region:no-drag]",
        )}
      >
        {/* The title stands on a 56px band over the page, as wide as the
            page's content so both ends line up with it: the way back and the
            page's name at the start, the page's actions at the end.
            At rest they are plain; once the page scrolls under them each end
            is a floating capsule (solid `surface`, `shadow-float`), the
            conversation header's. The capsule's room reaches outside the
            column, so the title itself never moves. */}
        <div
          data-scrolled={scrolled ? "" : undefined}
          className={cx(
            "settings-band group/band absolute inset-x-0 top-0 z-lift h-14 px-6 max-narrow:px-3",
            standalone ? "[-webkit-app-region:drag]" : "pointer-events-none",
          )}
        >
          <div className="mx-auto flex h-full w-full max-w-[680px] items-center gap-2">
            {/* Back is a round capsule of its own, as the conversation's
                is; the title is the next. */}
            <span
              className={cx(
                SETTINGS_CAPSULE,
                // Its arrow stands on the content's edge; a long title is
                // cut, never this.
                "-ms-2.5 size-10 shrink-0 justify-center max-narrow:-ms-2 max-narrow:size-11",
                !back && "hidden max-narrow:flex",
              )}
            >
              <IconButton
                variant="ghost"
                icon={ArrowLeft}
                aria-label={upLabel}
                disabled={backDisabled}
                onClick={back ?? onList}
                className="size-9 rounded-full [-webkit-app-region:no-drag] max-narrow:size-11"
              />
            </span>
            <div
              className={cx(
                SETTINGS_CAPSULE,
                // At rest the title follows Back closely, or starts where
                // the content does; once it is a capsule it takes its
                // padding (6px before a machine's icon, `data-page-title`,
                // as before a conversation's avatar), and with no Back it
                // reaches out so the words stay where they were.
                "pe-4 transition-[background-color,box-shadow,padding,margin] group-data-[scrolled]/band:ps-4 group-data-[scrolled]/band:has-[[data-page-title]]:ps-1.5",
                !back &&
                  "group-data-[scrolled]/band:-ms-4 group-data-[scrolled]/band:has-[[data-page-title]]:-ms-1.5 max-narrow:group-data-[scrolled]/band:ms-0 max-narrow:group-data-[scrolled]/band:has-[[data-page-title]]:ms-0",
              )}
            >
              <h2
                id={titleId}
                className="flex h-10 min-w-0 items-center text-title-2-semibold text-ink"
              >
                <span className="truncate">{heading ?? selected?.label}</span>
              </h2>
            </div>
            <div
              ref={setActions}
              className={cx(
                SETTINGS_CAPSULE,
                "-me-1 ml-auto shrink-0 justify-end px-1 empty:hidden [-webkit-app-region:no-drag]",
                // One icon alone is a round capsule, as wide as it is tall.
                "min-w-10 max-narrow:min-w-11 has-[>:only-child]:justify-center has-[>[data-slot=icon-button]:only-child]:px-0",
              )}
            />
          </div>
        </div>
        {/* Pages change at once: no motion between them. */}
        <section
          key={page}
          aria-labelledby={titleId}
          onScroll={(event) => {
            if (event.target === event.currentTarget)
              setScrolled(event.currentTarget.scrollTop > 4);
          }}
          className="settings-scroll min-h-0 flex-1 overflow-y-auto px-6 pb-6 pt-16 max-narrow:px-4 has-[.capability-editor,.settings-fill]:overflow-hidden has-[.capability-editor,.settings-fill]:pb-0"
        >
          <div
            data-page-column=""
            className="mx-auto w-full max-w-[680px] has-[.capability-editor,.settings-fill]:h-full"
          >
            <SettingsActionsSlot.Provider value={actions}>
              {children}
            </SettingsActionsSlot.Provider>
          </div>
        </section>
      </div>
    </div>
  );
}
