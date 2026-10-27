# Design foundations

Part of the [Veynode design system](../DESIGN.md). Product hierarchy, visual direction and materials, color, typography, the scales (spacing, sizes, radii, elevation, icons) and the breakpoints (sections 2–7). Where things stand on screen is the [Workspace shell](shell.md).

## 2. Product and information hierarchy

Veynode is a workspace for completing tasks through Bots, conversations, and work computers. Users should always understand **who they are working with, how work is progressing, what they need to do, and who controls the computer**.

- **Bot**: execution and communication identity. Keep avatar, name, and model settings consistently associated.
- **Group / conversation**: communication and collaboration context. Switching conversations does not implicitly change control permissions.
- **Computer**: execution environment. Connection, running state, and control ownership are separate.
- **Work**: objective, plan, execution records, and results. Completion needs actual evidence.
- **Control session**: current operator and valid control scope. Opening a preview does not acquire control.

Default hierarchy: current content → actionable next step → necessary status → optional process details and diagnostics. Place actions near their objects and keep one clear primary action per area. Put management information in settings or details so conversation reading remains central.

## 3. Visual direction

Veynode Soft: soft and glanceable, the Bots in front and the interface behind them. It is the same family as the screen-face Bot avatar: large radii, pill buttons, cool neutral greys, and flat, soft layers. The full brand book lives with the design system; this section is what the code holds to.

- **Neutral UI.** The chrome is ink and cool grey. The primary action, the focus ring, links, selection, switches and checkboxes are ink (black in light, near-white in dark). There is no brand hue: the old blue is only the blue Bot's colour. Colour comes from two places only: a Bot (its avatar colour, and a tint and ink for its chips, mentions and file icons) and a state (done, failed, waiting, working).
- **Glanceable.** Screens carry labels, values and controls, not explanatory sentences, subtitles or tutorial placeholders. An explanation goes behind an ⓘ (`InfoTip`); heads never carry one. One `primary` button per screen; the rest are `secondary` or `ghost`.
- **Flat.** Nothing in the page casts a shadow: panes, cards, the composer, buttons, selected rows, the segmented thumb and attachments are set off by fill or a 1px `line`, never by a shadow or a top highlight. Only what floats over the page has one: `float` for menus, listboxes, popovers, the @ suggestions, tooltips, toasts and the floating capsules below, `modal` for dialogs and sheets. A notice or card that floats over content is floating too: it takes `float`, or it moves into a head ([Heads](shell.md#heads)).
- **Materials.** Five, from the bottom up; an element is one of them. (0) The floor: the window. `canvas` with its wash, or the Mac window's own vibrancy; the sidebar and the Settings list stand on it with no fill. (1) The pane: `surface`, 80% of it on glass. (2) The veil: quiet fills inside a pane (a settings group, a hovered or selected row, a track): `surface-2`, and on glass a tint of the text colour (6%, 10% hovered), because an opaque grey on glass disappears. Inside a veil a hover is `ink` 5% and an icon plate or neutral tag `ink` 6%, so they show on a surface and on a veil alike. (3) The floating capsule: solid `surface` and `shadow-float`, flat, with no hairline, no gradient and no blur, the same on a phone. Only for what content scrolls under (the conversation header's capsules, Jump to latest, the conversation finder's field, a Settings page's title once the page has scrolled, a Space item's head, a phone's places). (4) Solid: dialogs, sheets, menus, tooltips and the finder's results keep an opaque `surface`, since they are read. Never glass on glass, never a blur on something at rest in the page, and a layer that covers content (a drawer below compact, a pane during a swap) is drawn solid.
- **Layers.** The canvas (`bg-canvas`) is the floor and the sidebar sits on it flush. In a browser the workspace floor is one flat tone (`canvas-wash`), so a Bot's grey bubble never reads as the floor; in the Mac app the floor is the window's frosted glass instead, with no wash. Inside a pane, quiet fills (a hovered or selected row, a code block, a segmented track) are `surface-2`; what a person types into is `field`. Settings stands in the same layers: its page list on the floor, the shown page's row on the surface fill, the page a pane, its rows in veil groups. Where the panes stand and how they collapse is the [shell](shell.md#columns-and-panes).
- **Messages are bubbles.** A Bot's reply is `bubble-bot` (light neutral grey, ink words); the person's own is `bubble-me` (neutral dark). Neither is coloured. Avatars and names show in the transcript only in a Group. Sizes, joins and widths are the [Message layout contract](message-layout.md#current-presentation).
- **Density.** Two densities share one size scale ([section 6](#6-spacing-sizes-radii-elevation-and-icons)): roomy for chat (the conversation, a Group, the Bot editor) and management (Settings, capabilities, nodes, virtual machines).
- Peer areas share padding, headings and action placement; groups are separated by space and, at most, one hairline, never by nested cards or a coloured side bar. A card is only for something that must be picked up on its own.
- Preserve the Veynode avatars and BotLoader. Decoration must not obscure content or imply execution progress. No marketing headings, full-screen posters, decorative gradients, or glass beyond the floor. No second icon set or UI library for one page.

## 4. Color

All colour comes from the tokens in [boardui-theme.css](https://github.com/Veynode/core/blob/main/packages/ui/src/styles/boardui-theme.css), whose values are the design system's `tokens.json`. Tokens already switch between light and dark (and re-resolve inside a subtree marked `dark`, such as the computer window), so feature code never writes `dark:`, a palette utility such as `neutral-200`, or a hex value.

Each Veynode Soft token is a Tailwind utility under its own name (`bg-surface`, `text-ink-2`, `border-line`, `inset-ring-line`, `bg-state-failed-tint`, `text-bot-blue-ink`). The semantic names older code uses are aliases of them (`text-text-primary` is `ink`, `bg-background-secondary-default` is `surface-2`); both are correct, and new code prefers the Soft names.

| Role                                                                                    | The one utility                                                                        | Alias older code uses                                     |
| --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| App floor behind the panes; the sidebar's ground                                        | `bg-canvas`                                                                            | `bg-background-canvas`                                    |
| Panes, cards, dialogs, the transcript, the composer                                     | `bg-surface`                                                                           | `bg-background-surface`, `bg-background-full`             |
| Quiet fill inside a surface: hovered or selected row, code block, segmented track, hint | `bg-surface-2`                                                                         | `bg-background-secondary-default`                         |
| Something a person types into: input, textarea, select, search                          | `bg-field`                                                                             | `bg-background-tertiary-default`                          |
| Hairline: dividers, card and group edges, a secondary button's edge (decorative only)   | `border-line` / `inset-ring inset-ring-line` / `divide-line`                           | `border-separator-border`, `border-border-button-default` |
| A control border that must be seen: checkbox, radio, an off switch's track              | `line-strong`                                                                          | `border-border-checkbox-default`                          |
| Primary text and active icons                                                           | `text-ink`                                                                             | `text-text-primary`                                       |
| Secondary text: labels, values, timestamps, idle icons                                  | `text-ink-2`                                                                           | `text-text-secondary`, `text-text-tertiary`               |
| Placeholders and disabled text only (under 4.5:1 by design)                             | `text-ink-3`                                                                           | `text-text-placeholder`                                   |
| The one strong control on a screen: primary button, checked control                     | `bg-action` + `text-on-action`                                                         | `bg-button-primary` (the Button owns it)                  |
| Keyboard focus                                                                          | `outline-2 outline-offset-2 outline-focus` (text controls: `shadow-field-ring`, below) | `ring-border-focus-ring`                                  |
| A Bot's reply bubble                                                                    | `bg-bubble-bot`                                                                        | `bg-message-incoming-background`                          |
| The person's own bubble and its words                                                   | `bg-bubble-me` + `text-on-bubble-me`                                                   | `bg-message-outgoing-background`                          |
| Behind dialogs and sheets                                                               | `bg-scrim`                                                                             | `bg-overlay-backdrop`                                     |
| Behind a picture shown on its own (see-through dark, either theme), its controls        | `bg-viewer` + `text-on-viewer`                                                         |                                                           |
| A state's mark (dot, check, destructive fill)                                           | `bg-state-{done,failed,waiting,working}`                                               |                                                           |
| A state's ground (notice, tag) and the words on it or on surface                        | `bg-state-*-tint` + `text-state-*-ink`                                                 | `text-text-error-primary` is `state-failed-ink`           |
| Text on a destructive (`state-failed`) fill                                             | `text-on-danger`                                                                       |                                                           |
| A Bot's body (avatar, member dot, group mosaic)                                         | `bg-bot-{blue,teal,green,yellow,orange,pink,purple,black}`                             |                                                           |
| That Bot's chip, @mention and file-icon ground, and the words on it                     | `bg-bot-*-tint` + `text-bot-*-ink`                                                     |                                                           |

Rules:

- **States** are always a dot or icon plus a word (`StateTag`, `Notice`, `SelectBadge`), never colour alone; a dot that stands without a visible word (a node's Online dot, the unread dot) carries the word as its accessible description. One state is one colour wherever it is shown: red is only failed, so a word such as Offline is the same colour in every list ([Tags and badges](components.md#tags-and-badges)). `state-waiting`'s amber is the avatar's "needs you" badge; it is under 3:1 on light grounds, so it always carries its dark glyph or its word, never a bare dot on white. Unknown states are neutral and show their raw word.
- **`ink-3` is for placeholders and disabled text only.** An icon or a word a person needs (a chevron, a Stop, a file's size, an empty state's icon) is `ink-2`. **Current:** the row chevron, the empty and failed states' icon, the progress line's Stop and several meta words use `ink-3`.
- **Bot colours** come from the Bot, never from what a thing is: black Bots follow the theme (black on light, white on dark).
- **Focus** is a 2px `focus` outline with a 2px gap, which meets 3:1 on every ground, and it is global: the `:focus-visible` rule in `packages/ui/src/styles/base.css` draws it on every button, link and `[data-focus-visible]` element and restores the outline style Tailwind's `outline-none` removes, so `outline-none` alone never hides the ring. Pointer focus never shows it. Anything else that takes focus (a message bubble) names the outline itself, with `outline-solid`; a menu item draws it inside its row (`-outline-offset-2`). A text control (input, textarea, select trigger, number field, the composer) instead turns from `field` to `surface` with the field ring: a 1px `line-strong` inset edge and a soft 3px ink halo (`shadow-field-ring`, or the `field-focus-within` utility on a shell); an invalid one draws the edge in `state-failed` (`shadow-field-invalid`, `shadow-field-invalid-ring` while focused).
- The `accent-*` ramp BoardUI used for interaction now resolves to ink and grey; do not add new `accent-*` uses.
- No `dark:` on any utility in feature code; no hex, `rgb()` or `oklch()` in class strings or `app.css`; no dynamic class construction such as `` `text-${tone}-500` ``. The eight Bot colours (`avatarColors` in `@veynode/shared/avatar`) reach the page only as inline styles on colour swatches and as the avatar's own drawing, never as class strings. The Bot avatar (`components/bots/robot/screen-face.ts`) is character art like BotLoader and the marks in `components/brand/`: its shapes, tile and colours are drawn inside its own SVG, never as class strings.
- All text reaches 4.5:1 on its ground in both themes; control borders (`line-strong`), icons and the focus ring 3:1. `line` is decoration only, never the only cue for a control. A token does not guarantee contrast on every background: inspect the actual combination.

## 5. Typography

Geist for the interface and the words (Chinese falls back to PingFang SC, HarmonyOS Sans SC, Microsoft YaHei); JetBrains Mono for code. Both ship with the app from npm (`@fontsource-variable/geist`, `@fontsource-variable/jetbrains-mono`, imported by [base.css](https://github.com/Veynode/core/blob/main/packages/ui/src/styles/base.css)), so they work offline and in the desktop app. Numbers are tabular everywhere. Sizes come only from the [typography recipes](https://github.com/Veynode/core/blob/main/packages/ui/src/styles/boardui-typography.css); a recipe carries size, line height, tracking and weight, and `-regular`/`-medium`/`-semibold` pick 400/500/600.

| Text role                                                                                                                          | The one recipe                                                                  |
| ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| The empty workspace's line and the empty conversation's welcome, at most one per screen (26/34, 650)                               | `text-title-1-semibold`                                                         |
| A page before the workspace: sign-in, `/setup`, the first-run card                                                                 | `text-title-1-semibold`                                                         |
| Dialog title (owned by `DIALOG_HEADER`), Settings page title (owned by `SettingsPage`), a page head's name (`PageSummary`) (20/28) | `text-title-2-semibold`                                                         |
| A column's head: sidebar (`SidebarHead`), the conversation name, a side editor; a card title (17/24)                               | `text-title-3-semibold`                                                         |
| Message text and the composer (15/24)                                                                                              | `text-chat-regular`                                                             |
| Default UI text: rows, fields, menus, values (14/20)                                                                               | `text-body-regular`                                                             |
| Names in lists, a row's leading label, buttons (Button owns it)                                                                    | `text-body-semibold` (`text-body-medium` for a settings row's label)            |
| Compact rows, list previews, a row's value and meta, a field's label (13/18)                                                       | `text-body-2-regular` (label: `text-body-2-semibold`, owned by `Label`)         |
| The label over a block: a field's control or a settings group (owned by `SettingsSection` and `Field`) (13/18)                     | `text-body-2-semibold text-ink` (`BLOCK_LABEL`)                                 |
| Timestamps, tooltips, hints, validation errors, the speaker's name in a Group (12/16)                                              | `text-caption-1-regular` / `-medium` / `-semibold`                              |
| A state word in a 20px badge (`StateTag`)                                                                                          | `text-caption-1-regular` (see [Tags and badges](components.md#tags-and-badges)) |
| Code, paths, commands, an approval's exact target (13/20)                                                                          | `font-mono text-code-regular`                                                   |

A Bot's reading column is 600px ([Message layout contract](message-layout.md#current-presentation)), about 36–40 Chinese characters. Banned in feature code: `text-[Npx]`, the Tailwind scale (`text-xs` … `text-9xl`), recipe names that do not exist (`text-body`, `text-headline-*`, `text-caption-2-*`, `text-display-*`), and a standalone `font-medium` / `font-semibold` next to a recipe. Do not add `leading-*` to a recipe unless a measurement contract requires the current metric. Stylesheets follow the same table: a size, weight or line height written in CSS (`card.css`, `markdown.css`) is the value of one of these recipes. **Current:** `base.css` gives every `<p>` a 1.8 line height, which overrides a recipe's line height on a paragraph; primitives undo it locally.

Let long explanations wrap. Truncated names must remain available in details. Errors, approval targets, and destructive consequences must not be reduced to an ellipsis. Avoid all caps, thin weights, or low opacity as hierarchy substitutes. How long each kind of text may be is the [copy budget](motion-copy-accessibility.md#copy-budget).

## 6. Spacing, sizes, radii, elevation, and icons

Every dimension comes from one of the scales below. Shared controls own their internal values; feature code does not restate them. An arbitrary value (`w-[300px]`, `py-[9px]`, `size-[38px]`, a px literal in a stylesheet) is allowed only where a measurement contract or a named width below needs it, and says which in a comment; `pnpm check:design` does not check sizes or spacing yet ([Enforcement](motion-copy-accessibility.md#enforcement)).

### Grid and alignment lines

One unit, three edges. The unit is 4px; every inset and gap is one of the steps in [Spacing](#spacing). The insets form a ladder, and each text block stands on the edge of its level, so text of one level shares one line:

- **Column edge, 12px** (`px-3`): the sidebar's list and its places control. A sidebar row's own inset is 8px (`px-2`), so its leading item starts at 20px, the sidebar head's mark line.
- **Pane edge, 16px** (`px-4`): a side pane, a settings group, a narrow page and a phone's gutter.
- **Page edge, 24px** (`px-6`): a Settings page's title band and body, a dialog's header, body and footer.

Vertical lines are shared too: the head centres on 36px from the window's top, and the composer and the side editor's footer centre on one bottom line. The conversation column is 800px wide. Alignment expresses information: text that belongs to one level starts on that level's edge, and a step in is a step down a level. An optical correction (an icon's glyph against its text, a round avatar's edge) is allowed where it makes the two read as aligned; a value that merely moves a box by a few pixels is not.

### Spacing

The rhythm is 4px: the steps are 2, 4, 6, 8, 12, 16, 20, 24 and 32px (`0.5`, `1`, `1.5`, `2`, `3`, `4`, `5`, `6`, `8`). 6px is only the label-to-block gap and a joined bubble's inner corner; 2px only between stacked rows of one list. Values off the scale (9, 10, 14, 15px) are debt, not a pattern: a row, menu item, notice, toast or tooltip uses 8 or 12, and a section or empty state uses 16, 24 or 32. The transcript's bubble padding (`py-[9px]`, `px-3.5`) is measured by the [message layout contract](message-layout.md) and stays as it is.

| Context                                            | The one value                                                                                                                                                 |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Icon ↔ label                                       | `gap-2` (`gap-1.5` inside a button, owned by `Button`); a row's leading icon or mark ↔ its name `gap-3` (owned by `ListRow`, `SettingsLinkRow`)               |
| Controls in a row                                  | `gap-2`; action rows wrap with `flex-wrap gap-2`                                                                                                              |
| Form fields                                        | `flex flex-col gap-4`; label ↔ control `gap-1.5`, owned by `Field`                                                                                            |
| Label ↔ the block under it; a group ↔ its footnote | 6px (`space-y-1.5`) and 8px, owned by `SettingsSection` and `SettingsGroup`                                                                                   |
| Rows in a settings group                           | `px-4`, hairlines inset 16px from each side, owned by `SettingsGroup`                                                                                         |
| Card, hint, notice interior                        | `px-3 py-2` for a notice or hint (owned by `Notice`), `p-4` for a card                                                                                        |
| A Settings page                                    | its parts 32px apart (`settings-page-body`), the page `px-6 pb-6` (narrow `px-4`) at most 680px wide, under the 56px title band; owned by `SettingsPage`      |
| A side pane (details, editors)                     | 16px edge ([Columns and panes](shell.md#columns-and-panes)); sections 24px apart                                                                              |
| Dialog header, body and footer                     | `px-6`, narrow `px-4`; owned by `DIALOG_HEADER`, the `dialog-body` recipe and `DialogFooter` (see [Dialog anatomy](components.md#dialog-anatomy))             |
| The shell                                          | panes 8px apart and 8px from the window's edge ([Columns and panes](shell.md#columns-and-panes))                                                              |
| Transcript rows                                    | 8px above a row, 4px above a row that continues a speaker's run (owned by `chat-exchange`, [Message layout contract](message-layout.md#current-presentation)) |

### Sizes

One height scale for everything a person presses or reads in a line. Things that share a line share a height ([Rows and controls](settings.md#rows-and-controls)).

| Size | What has it                                                                                                                                                                                                                         |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 20px | A badge (`StateTag`), a chip, an inline count                                                                                                                                                                                       |
| 28px | `small` controls: a row's button, the row ··· trigger (`RowMenu`, a 16px glyph), a label row's action, the dialog close, a card's send, a message's quick actions; an icon in it is 16px                                            |
| 32px | Plates: the conversation header's icon plates, the composer's buttons, the computer window's toolbar buttons. A row's own mark (a computer's icon, a logo, an avatar) and the slot a list's line icon is centred in (`ListRowIcon`) |
| 36px | `medium` controls, the default: a field, a `Select`, a button, pill tabs, a search box, a head's action, a menu or option row (44px on a phone)                                                                                     |
| 40px | A head's floating capsule, Jump to latest, the progress slot                                                                                                                                                                        |
| 44px | `large` controls and every touch target on a phone (the one exception: the mini computer's dock under the details' preview keeps its 32px buttons at every width); the composer's line; a sidebar row (Settings pages)              |
| 48px | A list row and a settings row (one line, `min-h-12`)                                                                                                                                                                                |
| 56px | A head; a conversation row; a staged file tile                                                                                                                                                                                      |
| 64px | A page head's icon (`PageSummary`); a side editor's footer                                                                                                                                                                          |

Avatars: 38px in the conversation list, 32px in the conversation header, 24px on a Group's rail; the progress character as the message layout contract says.

### Radii

Radius follows role, not size. These are the only radius utilities; `pnpm check:design` rejects every other (`rounded-2xl`, `rounded`, `rounded-[…]`).

| Element                                                                                                                | Radius                                                                         |
| ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Keyboard hint, inline code, checkbox, a question's single field                                                        | `rounded-xs` (6px)                                                             |
| Menu item, a small icon button's hover plate, compact select, a code block, a card's inner box (answer group, command) | `rounded-sm` (10px)                                                            |
| Input, select, list row, conversation row, file tile, notice, tooltip                                                  | `rounded-md` (14px)                                                            |
| Card, message bubble, popover, menu panel, settings group                                                              | `rounded-lg` (18px); the panes in the Mac window (concentric with its corners) |
| Pane, dialog, the composer, sheet                                                                                      | `rounded-xl` (24px), owned by `DIALOG_SURFACE` for dialogs                     |
| Button, badge (`StateTag`), chip, switch, pill tabs, avatar, status dot, a head's capsule                              | `rounded-full`                                                                 |

A text field (`md`) and a pill button (`full`) may share a line; within one control kind the radius never varies.

### Elevation

Flat by default. The only shadow utilities are `shadow-float` (menus, listboxes, popovers, the @ suggestions, tooltips, toasts, the floating capsules; owned by `MENU_POPOVER_SURFACE`, `Tooltip` and the toast), `shadow-modal` (dialogs and sheets; owned by `DIALOG_SURFACE`) and the field ring (`shadow-field-ring`, `shadow-field-invalid`, `shadow-field-invalid-ring`; owned by the text controls). Set in-page things off with `bg-surface-2`, `bg-field` or a 1px `inset-ring inset-ring-line` / `border-line`; a selected row changes only its fill. No `shadow-[…]`, no top highlights. Layering belongs to the shared portals, dialogs and toasts; do not raise a feature z-index or place overlays inside clipping scroll containers. Where a feature must stack inside its own pane, use the named layers (`z-base` 0, `z-lift` 10, `z-float` 20, `z-cover` 30, `z-menu` 50, `z-top` 100; `boardui-theme.css`), never a number.

### Icons

Imported from `@veynode/ui/icons` only. It is lucide's set with Veynode's own drawing wherever one exists (`packages/ui/src/icons/drawings.ts`: chats, settings, node, Bot, monitor, chip, channels, folder, shapes), each on lucide's 24 grid under lucide's name, so redrawing one changes no call site. Stroke 1.5, round caps and joins; `svg.lucide` gets the stroke in packages/ui `base.css`, so call sites pass no `strokeWidth`, and `pnpm check:design` rejects another icon set, a direct lucide-react import or another stroke on an icon component. The sidebar foot draws the place shown as its solid icon (`MessageCircleSolid`, `SettingsSolid`, and `ShapesSolid` for Space, where the computers and files are), the others as their outline.

| Size | Where                                                                                                                                      |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| 20px | Default: an icon-only `medium` button, a head's action, a sidebar row's icon, a list or settings row's leading icon, a large button's icon |
| 16px | A labelled button's icon (owned by `Button`), a `small` icon button, a menu item (owned by `MENU_ITEM`), inline with body text             |
| 14px | Event rows and system lines, inline with caption text, a validation error's mark                                                           |

`Notice` draws its icon at 20px and the row ··· at 16px; the 28px state icon (owned by `EmptyState` and `PanelLoadError`) is the one exception to these sizes.

A computer's icon (`ComputerIcon`) is a drawing, not a line icon (`packages/console/src/components/computer/computer-stickers.tsx`): a flat device in one of the eight Bot colours (its kind's own colour when none was chosen), with grey bezels, stands and keyboards; from 28px it is a sticker (a white die-cut edge, one short shadow, a band of light, a fixed lean per kind), smaller it is the flat drawing; stopped or unreachable it is grey, and while a Bot works it glows in its colour (drawings that are not icons, such as progress rings, loaders and the toast marks, are plain SVG). A row's line icon stands bare, never on a plate; a computer's own icon, a logo and an avatar are a row's mark at 32px ([Shell](settings.md#shell)). Icons follow the text colour: idle `ink-2`, active `ink`. Icon size and hit area are separate: standalone desktop icon buttons are at least 28×28px, important touch actions 44×44px, enlarged at the component level. Icon-only buttons always have a localizable accessible name (also their tooltip). No emoji.

## 7. Workspace layout and responsiveness

Where the columns, panes and heads stand is the [Workspace shell](shell.md#workspace-shell). This section owns the breakpoints.

Three named breakpoints are declared in [base.css](https://github.com/Veynode/core/blob/main/packages/ui/src/styles/base.css) and are the only responsive variants feature code may use:

| Name           | Width    | Meaning                                                                                   |
| -------------- | -------- | ----------------------------------------------------------------------------------------- |
| `max-narrow:`  | < 600px  | One column at a time; dialogs and settings collapse to one column; rows keep their height |
| `max-compact:` | < 950px  | The details pane floats over the conversation                                             |
| `max-wide:`    | < 1150px | The details pane narrows; the sidebars keep their width                                   |

`max-[Npx]:`, `min-[Npx]:` and the Tailwind defaults (`sm:`, `md:`, …) are banned. **Current:** forms that lay fields side by side use container breakpoints, `@min-[440px]` and `@min-[640px]`, a bubble hides its quick actions under a 30rem container, and a memory waiting for review answers with ✕ and ✓ under 440px; these are the only container sizes.

- Keep [ResizableSidebar](https://github.com/Veynode/core/blob/main/packages/console/src/components/workspace/ResizableSidebar.tsx) and [ChatWorkspace](https://github.com/Veynode/core/blob/main/packages/console/src/components/workspace/ChatWorkspace.tsx) width constraints. Preserve saved widths while respecting available container space.
- Narrow-screen editors use shared Modal. Preserve back/close paths and drafts.
- Wrap or stack action rows when needed; never clip the primary button. Code, tables, and remote desktops may scroll locally without causing page-wide horizontal overflow.
- Keep input and send controls reachable when the virtual keyboard opens. Use existing dynamic viewport handling rather than a fixed screen height that hides content.

Check layout changes on desktop and narrow screens. For breakpoint changes, check both sides of each affected breakpoint. Optional scenarios include 390px, 768px, 1280px, 1440px, and 200% zoom.
