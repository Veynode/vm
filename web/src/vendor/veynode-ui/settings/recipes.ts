import {
  DIALOG_SURFACE,
  DIALOG_HEADER,
} from "../boardui/overlays/dialog-styles";
import { cx as twMerge } from "../boardui/utils/cx";

/** Reusable Tailwind utility recipes. Marker classes are only DOM/state hooks.
 * Responsive rules use the named breakpoints from `lib/breakpoints.ts` (narrow/compact/wide). */
export const ui: Record<string, string> = {
  // The root of every Settings page (design-system/settings.md, "Shell"):
  // its groups 32px apart in the page's body text. A page that is one form
  // (`settings-fill`) fills the pane so its actions stay pinned at its foot.
  "settings-page-body":
    "flex w-full min-w-0 flex-col gap-8 text-body-regular text-ink has-[.settings-fill]:h-full",
  "capability-section": "min-w-0 space-y-3",
  "capability-heading": "text-body-medium text-ink",
  "capability-list": "divide-y divide-line",
  "capability-scroll-list":
    "max-h-80 overflow-y-auto overscroll-contain rounded-lg border border-line",
  "capability-row":
    "flex min-h-12 min-w-0 items-center justify-between gap-3 py-2",
  row: "flex items-center gap-2",
  muted: "text-caption-1-regular text-ink-2",
  // A card set apart by its hairline, never a shadow.
  panel: "rounded-lg bg-surface inset-ring inset-ring-line",
  hint: "flex items-start gap-2 rounded-md bg-surface-2 px-3 py-2 text-ink-2 [&_svg]:shrink-0 [&_p]:text-body-2-regular",
  "chat-app":
    "group/app flex h-dvh min-h-0 gap-2 overflow-hidden bg-canvas py-2 pr-2 text-body-regular text-ink max-narrow:gap-0 max-narrow:p-0",
  "chat-sidebar":
    "flex w-[300px] shrink-0 flex-col bg-canvas max-narrow:w-full max-narrow:group-[.has-conversation]/app:hidden",
  "conversation-list":
    "mx-1 min-h-0 flex-1 overflow-auto px-2 pt-2 pb-16 max-narrow:pb-20 [overflow-anchor:none]",
  conversation:
    // A roomy 56px row (ListRow comfy): the current one takes the surface's
    // fill, never a shadow; hover is a quiet surface-2. The unread dot is cut
    // out of the avatar by a ring in the row's own ground (`--mark-ring`).
    "[--mark-ring:var(--color-canvas)] max-narrow:[--mark-ring:var(--color-surface)] hover:[--mark-ring:var(--color-surface-2)] [&.selected]:[--mark-ring:var(--color-surface)] relative mb-0.5 flex h-14 overflow-hidden w-full flex-row items-center justify-start gap-3 rounded-md px-2 py-2 text-left text-ink transition-colors duration-120 hover:bg-surface-2 [&.selected]:bg-surface [&[data-unread]_.conversation-preview]:text-ink [&[data-unread]_time]:text-ink",
  "conversation-copy": "flex min-w-0 flex-1 flex-col",
  "conversation-title":
    "flex items-center gap-2 text-body-semibold [&_time]:ml-auto [&_time]:whitespace-nowrap [&_time]:text-caption-1-regular [&_time]:text-ink-2",
  "conversation-preview": "block truncate text-body-2-regular text-ink-2",
  "chat-avatar":
    "inline-flex bg-field rounded-full size-[38px] shrink-0 items-center justify-center overflow-hidden rounded-full text-[23px] leading-none [&:has(>[data-robot-avatar])]:overflow-visible [&:has(>[data-robot-avatar])]:bg-transparent",
  "sidebar-hint": "px-2 py-8 text-center text-body-regular text-ink-2",
  "chat-profile":
    "flex w-full items-center gap-3 rounded-md px-2 py-2 text-body-medium hover:bg-surface-2 [&_small]:mt-0.5 [&_small]:block [&_small]:text-caption-1-regular [&_small]:text-ink-2",
  "user-avatar":
    // The person's own mark wears their colour, the one their messages carry.
    "grid size-8 shrink-0 place-items-center rounded-full bg-bubble-me text-caption-1-medium text-on-bubble-me uppercase",
  "chat-main":
    // A flat pane on the canvas: the surface's fill, no shadow.
    "relative m-0 flex min-w-0 flex-1 flex-col overflow-clip rounded-xl bg-surface max-narrow:rounded-none max-narrow:hidden max-narrow:group-[.has-conversation]/app:flex",
  // The pane's header floats over the transcript: no bar and no fade, two
  // capsules (the name, the icon group) on a 56px band. The band's own box
  // is the inset VirtualMessages starts the list under, so it never changes
  // height. The messages scroll under it; the capsules are floating layers
  // (solid `surface`, `shadow-float`), so they stand clear of the grey and
  // the dark bubbles alike (`chat-pill` is also the hook a pane swap moves,
  // base.css). The capsules line up with
  // the composer's box (`chat-compose`: 832px less its 16px sides) once the
  // pane is wider than it, so the header, the messages and the composer read
  // as one column.
  "chat-header":
    "pointer-events-none absolute inset-x-0 top-0 z-float flex h-14 min-h-14 shrink-0 items-center gap-2 px-[max(12px,calc((100%_-_800px)/2))] max-narrow:ps-1.5 max-narrow:pe-1.5 [&_h2]:text-title-3-semibold [&_.chat-avatar]:size-8 [&_[data-group-avatar]]:size-8 max-narrow:[&_.chat-pill]:h-11",
  // A control floating over the transcript (the header's pills, a Space
  // item's): solid `surface` and the float's drop, no hairline and no blur.
  // The shadow holds on a ghost Button, whose own shadow-none merging keeps.
  "chat-pill":
    "pointer-events-auto flex h-10 min-w-0 shrink-0 items-center rounded-full bg-surface shadow-float! transition-[background-color] duration-200",
  // A header's icon button inside its capsule: a 20px icon in ink-2 on a
  // 32px plate (44px on a phone) that darkens on hover and press; a toggle
  // keeps the darker plate while what it opens is open (the details pane).
  "chat-header-action":
    "size-8 rounded-full border-0 p-0 text-ink-2 transition-colors hover:bg-ink/6! hover:text-ink active:bg-ink/10! aria-expanded:bg-surface-2! aria-expanded:text-ink [&_svg]:size-5 max-narrow:size-11",
  "mobile-back": "hidden! max-narrow:flex! max-narrow:size-11",
  // The space sits above a row, not under it, so what arrives next never
  // resizes the row before it. A row continuing the same speaker's bubbles
  // sits closer (`bubbleRuns`).
  // A row is the container a system line spans (MessageNotice `center`).
  // A continuing row is a container too: its notices span it (`100cqw`).
  "chat-exchange": "@container pt-2",
  "chat-exchange-continued": "@container pt-1",
  // A small chip rather than a floating line; 32px tall (the layout contract's
  // notice counts on it). One 16px line in a 20px box: a name too long for the
  // row ends in an ellipsis, it never wraps to a second line.
  "message-time":
    "mx-auto mt-2 mb-1 block h-5 w-fit max-w-full truncate px-2 py-0.5 text-center text-caption-1-medium leading-4 tracking-normal text-ink-2",
  "message-line":
    // A Group's rail: a 24px avatar and 12px to the bubble (rowOf's
    // `--column-offset` is their sum), centred on the bubble's first line.
    "my-0 flex items-start gap-3 [&~.message-line]:mt-2 [&>.chat-avatar]:mt-6 [&>.chat-avatar]:size-6",
  outgoing:
    "justify-end [&>.bubble]:border [&>.bubble]:border-bubble-me [&>.bubble]:bg-bubble-me [&>.bubble]:text-on-bubble-me",
  // The Bot's words share the fill of the card it asks with, so a reply and a
  // question from the same Bot read as one voice; outgoing bubbles override it.
  bubble:
    "w-fit min-w-0 max-w-full rounded-lg bg-bubble-bot text-ink px-3.5 py-[9px] text-chat-regular whitespace-pre-wrap [overflow-wrap:anywhere]",
  // A card fills the column, as it does in a direct conversation, so its
  // width never follows its answer area: sized to its content, a settled card
  // narrowed once the options it no longer offers were gone.
  // A delivered file is one of the Bot's bubbles: the same fill and corners,
  // its icon on a light tile instead of a border.
  "file-bubble":
    "flex min-w-[240px] items-center gap-2 rounded-lg bg-bubble-bot px-3 py-2 max-narrow:min-w-0",
  "bot-reply":
    // A Bot's words in a Group read in the same 450px column as in a direct
    // chat (`max-w-[min(450px,100%)]`, the column less the avatar rail); the
    // bubble's own actions hang off its right edge. A card fills the column.
    "min-w-0 max-w-[min(450px,100%)] has-[.message-card]:w-full [&>small]:mb-1 [&>small]:block [&>small]:h-4 [&>small]:truncate [&>small]:text-caption-1-regular [&>small]:text-ink-2",
  "message-actions":
    "mt-2 flex flex-wrap items-center gap-2 text-caption-1-regular text-ink-2 [&_.tag]:px-1.5 [&_.tag]:py-0.5 [&_.tag]:text-caption-1-regular [&_button]:text-caption-1-regular [&>span[role]]:w-full [&>span[role]]:text-state-failed-ink",
  "composer-activity-slot":
    "relative z-float mb-2 h-7 min-h-7 min-w-0 shrink-0",
  "chat-compose":
    "absolute bottom-0 left-1/2 -translate-x-1/2 isolate mx-auto flex w-full max-w-[832px] shrink-0 flex-col justify-end px-4 pb-2 pointer-events-none [&>*]:pointer-events-auto max-narrow:px-3 max-narrow:pb-2.5 [&>small]:mt-2.5 [&>small]:block [&>small]:text-center [&>small]:text-caption-1-regular [&>small]:text-ink-2",
  composer:
    "m-0 rounded-xl border-0 bg-surface px-2 py-1.5 inset-ring inset-ring-line [&_textarea]:min-h-[52px] [&_textarea]:w-full [&_textarea]:resize-none [&_textarea]:border-0 [&_textarea]:p-2 [&_textarea]:text-sm [&_textarea]:shadow-none",
  "conversation-welcome":
    "mx-auto max-w-sm px-4 py-8 text-center text-ink-2 [&_.chat-avatar]:mb-6 [&_.chat-avatar]:size-12 [&_h2]:mb-2 [&_h2]:text-title-1-semibold [&_h2]:text-ink [&_p]:text-body-2-regular",
  // The empty workspace: the logo, one line and the one next step.
  "chat-empty":
    "flex flex-1 flex-col items-center justify-center p-8 text-center [&_h1]:mb-6 [&_h1]:max-w-sm [&_h1]:text-title-1-semibold [&_h1]:tracking-[-.6px]",
  "conversation-details":
    // A flat pane beside the chat; below the compact breakpoint it floats
    // over it, opened from the header, and only then casts a shadow; on a
    // phone it is a page of its own over the chat. Its sections are compact
    // settings groups, each under the one block label (`BLOCK_LABEL`).
    "relative flex w-[282px] shrink-0 flex-col rounded-xl bg-surface max-compact:fixed max-compact:inset-y-2 max-compact:right-2 max-compact:z-float max-compact:w-[280px] max-compact:max-w-[calc(100vw-16px)] max-compact:shadow-float max-narrow:inset-0 max-narrow:w-full max-narrow:max-w-none max-narrow:rounded-none max-narrow:shadow-none",
  // The details pane scrolls inside its body (ResizableSidebar
  // `bodyClassName`), so its resize handle can stand in the gap beside it.
  "conversation-details-body":
    "min-h-0 flex-1 overflow-auto rounded-xl px-4 pb-4 pt-0 has-[[data-routine-editor]]:*:invisible",
  "conversation-details-editor":
    "flex min-h-0 flex-1 flex-col overflow-clip rounded-xl",
  "message-context-chip":
    "inline-flex h-7 min-w-0 max-w-full items-center gap-1.5 rounded-sm bg-current/15 px-2 text-body-2-regular [&>span]:truncate [&>svg]:size-3.5 [&>svg]:shrink-0 [&>svg]:opacity-80 [&:is(button)]:cursor-pointer [&:is(button)]:outline-none [&:is(button)]:transition-colors [&:is(button)]:duration-150 [&:is(button):hover]:bg-current/25 [&:is(button):focus-visible]:ring-2 [&:is(button):focus-visible]:ring-focus",
  "section-heading":
    "mb-6 flex flex-wrap items-center justify-between gap-4 [&>div]:min-w-0 [&>div]:flex-1 [&>button]:shrink-0 max-narrow:flex-col max-narrow:items-start [&_h1]:text-title-2-semibold [&_h1]:text-ink [&_p]:text-body-regular [&_p]:text-ink-2",
  overlay:
    "fixed inset-0 z-cover flex items-center justify-center bg-scrim pl-[calc(1rem+var(--safe-left,0px))] pr-[calc(1rem+var(--safe-right,0px))] pt-[calc(1rem+var(--window-controls-inset,0px)+var(--safe-top,0px))] pb-[calc(1rem+var(--safe-bottom,0px))]",
  modal:
    DIALOG_SURFACE +
    " group/modal w-[480px] [&.sm]:w-[400px] [&.lg]:w-[640px] [&.xl]:w-[880px] max-w-full max-h-[min(88dvh,calc(100dvh-32px-var(--window-controls-inset,0px)-var(--safe-top,0px)-var(--safe-bottom,0px)))] overflow-auto has-[>.modal-head]:flex has-[>.modal-head]:flex-col has-[>.modal-head]:overflow-hidden [&.wide]:h-[min(820px,calc(100dvh-40px-var(--window-controls-inset,0px)-var(--safe-top,0px)-var(--safe-bottom,0px)))] [&.wide]:w-[min(1200px,calc(100vw-40px))] [&.wide]:max-h-[calc(100dvh-40px-var(--window-controls-inset,0px)-var(--safe-top,0px)-var(--safe-bottom,0px))] max-narrow:[&.wide]:h-[calc(100dvh-32px-var(--window-controls-inset,0px)-var(--safe-top,0px)-var(--safe-bottom,0px))] max-narrow:[&.wide]:w-[calc(100vw-20px)] max-narrow:[&.wide]:max-h-[calc(100dvh-32px-var(--window-controls-inset,0px)-var(--safe-top,0px)-var(--safe-bottom,0px))]",
  "modal-head": DIALOG_HEADER,
  "modal-body":
    "min-h-0 overflow-auto overscroll-contain has-[>.entity-form]:flex has-[>.entity-form]:flex-col has-[>.entity-form]:overflow-hidden has-[>.step-dialog]:flex has-[>.step-dialog]:flex-col has-[>.step-dialog]:overflow-hidden has-[>.capability-dialog]:flex has-[>.capability-dialog]:flex-1 has-[>.capability-dialog]:flex-col has-[>.capability-dialog]:overflow-hidden group-[.wide]/modal:flex group-[.wide]/modal:flex-1 group-[.wide]/modal:flex-col group-[.wide]/modal:overflow-hidden",
  "entity-form": "flex min-h-0 flex-col gap-0 overflow-hidden p-0",
  "entity-form-body":
    "flex min-h-0 flex-col gap-4 overflow-auto overscroll-contain px-6 pb-4 pt-1 max-narrow:px-4",
  "dialog-body":
    "flex flex-col gap-4 px-6 pb-2 pt-1 text-body-regular max-narrow:px-4 last:pb-6",
  // Pinned actions: no hairline; what scrolls under them fades out over the
  // 24px above the row (a `surface` gradient drawn outside the footer, so the
  // footer keeps its height and nothing under it takes a click). The fade is
  // only there while there is more to scroll to (app.css, `.modal-footer`), so
  // it never washes out the last field of a dialog that fits.
  "modal-footer":
    "sticky bottom-0 z-lift flex shrink-0 flex-wrap items-center justify-end gap-2 bg-surface px-6 pb-6 pt-3 max-narrow:px-4 max-narrow:pointer-coarse:[&_button]:min-h-11 before:pointer-events-none before:absolute before:inset-x-0 before:bottom-full before:h-6",

  "management-panel": "mb-6 p-5 [&_p]:leading-[1.7] [&_p]:text-ink-2",
  "management-row":
    "flex flex-wrap items-center justify-between gap-3 border-b border-line py-3 [&_small]:mt-1 [&_small]:block [&_small]:opacity-65",
  "bot-portrait": "block size-full object-cover [&:is(img)]:rounded-full",
  "avatar-picker-panel":
    "overflow-hidden rounded-xl border border-line bg-surface",
  "avatar-picker-tabs":
    "flex gap-2 border-b border-line p-3 [&>button]:rounded-sm [&>button]:px-3 [&>button]:py-2 [&>button]:text-ink-2 [&>button[aria-pressed=true]]:bg-surface-2 [&>button[aria-pressed=true]]:text-ink",
  "avatar-reset": "ml-auto",
  "avatar-shapes":
    "grid grid-cols-4 gap-2 p-3 [&>button]:size-14 [&>button]:justify-self-center [&>button]:rounded-full [&>button]:border-2 [&>button]:border-transparent [&>button]:p-0 [&>button[aria-pressed=true]]:border-line",
  "avatar-colors":
    "grid grid-cols-6 gap-4 px-6 pt-2 pb-6 [&>button]:size-8 [&>button]:justify-self-center [&>button]:rounded-full [&>button[aria-pressed=true]]:outline-2 [&>button[aria-pressed=true]]:outline-offset-4 [&>button[aria-pressed=true]]:outline-accent-500",
  "avatar-drop":
    "m-5 flex flex-col items-center justify-center gap-4 rounded-lg border-2 border-dashed border-line px-3 py-8 text-center text-ink-2 [&_small]:text-caption-1-regular",
  "desktop-session":
    "flex min-h-0 flex-1 flex-col overflow-hidden bg-surface-2",
  "desktop-status":
    "flex h-8 shrink-0 items-center gap-2 overflow-hidden border-b border-line bg-surface-2 px-5 text-caption-1-regular text-ellipsis whitespace-nowrap text-ink-2 [&>i]:size-[5px] [&>i]:shrink-0 [&>i]:rounded-full [&>i]:bg-ink-2",
  "connection-connecting": "[&>i]:bg-accent-400",
  connecting: "[&>i]:bg-accent-400",
  "connection-connected": "[&>i]:bg-green-500",
  "connection-error": "[&>i]:bg-red-400",
  "desktop-stage": "relative min-h-0 flex-1 overflow-hidden bg-zinc-900",
  "computer-dock":
    "flex max-w-full items-center gap-1 overflow-x-auto overflow-y-hidden rounded-full bg-surface-2 p-1 max-narrow:w-full max-narrow:justify-between",
  // Under the details' mini computer: the dock's look at every width, never the phone's full row.
  "computer-dock-mini":
    "flex w-fit max-w-full items-center gap-1 overflow-x-auto rounded-full bg-surface-2 p-1",
  "computer-dock-status":
    "flex shrink-0 items-center gap-2 pl-3 pr-1 text-caption-1-medium text-ink-2 max-narrow:pl-2 [&>i]:size-2 [&>i]:shrink-0 [&>i]:rounded-full [&>i]:bg-ink-2 [&>i[data-held]]:bg-state-done",
  "computer-dock-divider": "mx-1 h-5 w-px shrink-0 bg-line",
  "desktop-mode":
    "flex shrink-0 items-center gap-1.5 rounded-full bg-surface-2 px-2 py-1 text-caption-1-medium text-ink-2 max-narrow:px-1.5 [&>i]:size-2 [&>i]:shrink-0 [&>i]:rounded-full [&>i]:bg-ink-2 [&>i]:transition-colors [&>i]:duration-200 [&>i[data-held]]:bg-state-done [&>span]:whitespace-nowrap motion-reduce:[&>i]:transition-none",
  "desktop-hint":
    "pointer-events-none absolute bottom-4 left-1/2 z-lift flex max-w-[calc(100%-2rem)] -translate-x-1/2 items-center gap-1.5 rounded-full bg-scrim px-3 py-1.5 text-caption-1-medium text-text-white transition-opacity duration-200 [&>span]:truncate",
  vnc: "absolute inset-0 size-full overflow-hidden bg-zinc-900",
  "vnc-readonly":
    "cursor-default! [&_canvas]:pointer-events-none [&_canvas]:cursor-default!",
  "desktop-placeholder":
    "absolute inset-0 flex flex-col items-center justify-center gap-3 bg-surface-2 p-6 text-center text-ink-2 [&_h3]:text-body-semibold [&_h3]:text-ink [&_p]:max-w-[360px] [&_p]:text-caption-1-regular",
  "auth-form":
    "flex w-full max-w-[360px] flex-col gap-6 [&_h2]:text-center [&_h2]:text-title-2-semibold [&_h2]:text-ink max-narrow:[&_h2]:text-title-1-semibold [&_label]:text-body-medium [&_label]:text-ink-2 max-narrow:[&_label]:text-chat-medium [&>p]:text-center [&>p]:text-body-2-regular [&>p]:text-ink-2 [&>small]:text-center [&>small]:text-caption-1-regular [&>small]:text-ink-2",
  loading: "grid h-dvh place-items-center text-body-regular text-ink-2",
};
/** Resolve state-dependent recipe names. The names remain available as DOM hooks. */
export function classes(tokens: string) {
  return twMerge(
    tokens
      .split(/\s+/)
      .filter(Boolean)
      .map((token) => [token, ui[token] || ""].join(" "))
      .join(" "),
  );
}
