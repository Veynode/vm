# BoardUI contract

Part of the [Veynode design system](../DESIGN.md). The BoardUI sources, how controls compose, the dialog motion contract, menu and side pane motion, dialog keyboard ownership. How each component looks is [Components](components.md); the timings are the [motion table](motion-copy-accessibility.md#10-motion-and-feedback).

## BoardUI contract

Use the shadcn/ui components on Base UI in `packages/ui/src/components/ui/` (drawn in Veynode Soft) and the BoardUI theme, composed with `packages/console/src/components/ui/recipes.ts` and the primitives in `packages/console/src/components/ui/`. Their sources are recorded in [shadcn-sources.json](https://github.com/Veynode/core/blob/main/packages/ui/shadcn-sources.json); retain [SHADCN-NOTICE.md](https://github.com/Veynode/core/blob/main/packages/ui/SHADCN-NOTICE.md) and [BOARDUI-NOTICE.md](https://github.com/Veynode/core/blob/main/packages/ui/BOARDUI-NOTICE.md). Application components are local compositions, not newly imported upstream source.

### Controls and application composition

- Buttons and links: `Button`, `ButtonLink`, `IconButton`; dismiss: `CloseButton`; states: `StateTag`; generic labels: `Badge`. `ghost` is transparent at rest, `secondary` is the quiet fill; never simulate ghost by overriding another variant's background.
- Fields: `Input`, `Textarea`, `Label` / `Field`, `Checkbox`, `Switch`, `Select`, `Tabs`. Preserve native names, required and disabled semantics, label association and Select's `onValueChange` adapter.
- Menus and tooltips: Base UI `DropdownMenu`, `Popover` and `Tooltip`. `TooltipTrigger` retains tooltips on press by default, including copy feedback; pointer leave, blur and Escape dismiss them. Two application controls stay on React Aria: `DetailTip` (hover previews without taking focus; a press pins it as a dialog and moves focus in) and the volume `Slider` in `DesktopSoundControl` (`packages/console/src/components/computer/DesktopAudioControls.tsx`). No other control uses it, and application code never uses React Aria's `Modal` or `ModalOverlay`.
- The composer stays one surface with its existing editor and send control; do not wrap it in another `Input` / `Textarea` shell. Search uses an input icon slot, not nested borders.
- `ToastProvider` draws the toast (an ink pill: [Toasts](components.md#toasts)) in the native popover top layer. It is the former compact notification, restyled; it is not the inline `Alert`. Persistent action-local errors use `InlineError` or `Notice`, never a disappearing toast alone. Keep transient notifications outside virtual rows and preserve action-local pending states.
- Conversation layout, avatars, file flows, remote desktop and resizable sidebars are application-specific. The theme loads before rendering to avoid a light flash.
- Markdown code blocks preserve literal source while lazy highlighting loads. Tokens render as React text or spans, never raw HTML; unsupported or large sources stay plaintext. Copy feedback lasts 1.5 seconds without changing metrics. A block is a 30px header (the language label and Copy) over `pre`, which never wraps its lines but scrolls sideways, takes 12px of padding above and below and 12px type on 21px lines, and is capped at 400px, then scrolls inside. A table is capped at 280px and scrolls inside; its cells wrap.
- A Mermaid code block is one fixed box, 240px under its 30px header, whether it is drawn or not, so nothing moves when the drawing appears. It is drawn only once its reply has finished and the block is near the viewport, and never from a source over 20,000 characters; until then (and while the reply is still being written) the box holds a quiet placeholder. Once drawn, the header's language label gives way to Diagram / Source tabs, an enlarge button (28px, a 44px hit area on a phone, `markdownMessage.enlargeDiagram`) and Copy: the drawing scales to fit inside the box, Source is the code block's rules inside the same box, and Copy always copies the source. Clicking the box or the enlarge button opens the full-size diagram in a dialog (`Modal`, `size="wide"`, the shared dialog lifecycle). The drawing and the chosen tab live in the row's view state (`useMessageViewState`), so a remount shows the same panel without a reflow. A source Mermaid cannot parse, or one over the limit, shows that source in the same box, with no tabs and no error line.
- Capability overrides, operation permissions and exact-call approvals remain separate. An approval card keeps its exact target whole and its buttons answer the original authorization request.

### Mandatory dialog motion contract

All dialogs, confirmations, editors, settings, file and image previews and nested dialogs MUST:

1. Reuse the shared lifecycle in `packages/ui/src/boardui/overlays/dialog-motion.tsx` through `packages/console/src/components/ui/Modal.tsx`, `packages/console/src/components/ui/ViewerOverlay.tsx`, the shared `Dialog` (`packages/ui/src/components/ui/dialog.tsx`, shadcn/ui on Base UI, kept open and mounted until the shared exit ends; `ViewerOverlay` is built on it) or the registered native hook. Appearance-only `DIALOG_SURFACE` is insufficient. Never use React Aria's `Modal` or `ModalOverlay` in application components.
2. Animate opacity and scale only: a 200ms entrance and a 120ms exit (`DIALOG_MOTION`), never longer than 200ms. The one exception is a phone's sheet (a `Modal` or a preview's `ViewerOverlay` below 600px), which slides up from the screen's foot in 200ms and back down in 160ms (`sheetExit`), its scrim fading. Never animate layout dimensions or clone DOM.
3. Keep the actual subtree and content mounted until exit finishes for X, Cancel, Escape, backdrop, submit success and externally controlled close. Use `DialogPresence` **outside** conditional owners; never clear preview or editor content before exit.
4. Keep scroll locking and focus containment through exit, then restore focus without stealing it from a newer dialog. Nested closes preserve parents. Focus returns to what held it when the dialog opened (the shared `Dialog` does it, unless a newer dialog already holds focus); pass `restoreFocusRef` to `ViewerOverlay` for a trigger that does not hold focus when it opens.
5. Handle repeated close requests, reopen during exit and animation cancellation without stale callbacks closing a reopened surface. No call-site timers. Native dialogs prevent the default cancel and use the shared hook.
6. Respect reduced motion with immediate dismissal and no artificial delay.

```tsx
<DialogPresence>
  {open && (
    <Modal title={title} close={() => setOpen(false)}>
      {content}
    </Modal>
  )}
</DialogPresence>
```

`pnpm check:design` (rule `dialog-motion`) rejects a shell that skips the shared motion hooks. Dialog behaviour, lifecycle or presentation changes beyond the isolated copy/color exception run `pnpm test:browser dialogs`; the browser check must cover entrance, exit-before-unmount, content retention, all close paths, nested focus, rapid reopen and reduced motion. Transcript changes also follow the scroll verification in [Required verification](message-layout.md#required-verification).

### Menus and sidebar

Menus open in 140ms from the trigger or click point (up 4px, from .97) and close in 100ms, fading only (`MENU_POPUP`'s `data-starting-style` / `data-ending-style`; the [motion table](motion-copy-accessibility.md#10-motion-and-feedback)). Native menus use `useNativeMenuMotion`; conditional suggestions use `MenuPresence` (`packages/ui/src/boardui/overlays/`). Preserve Escape, outside press, item selection and rapid reopen. Reduced motion skips transitions.

`ChatWorkspace` owns one `ResizableSidebar` for conversation and computer details and for Bot and group editing. Desktop editors render content only; narrow layouts use Modal. Closing sets `open=false`; clear editor content only from `onClosed`. Switching contents preserves width; reversing an exit cancels the stale completion (`tests/browser/right-sidebar-motion-browser.mjs`).

The Bot and Group editors' contents are [Bot and Group editors](interactions.md#bot-and-group-editors).

**Settings** is a page over the workspace, not a dialog; its shell, Back and Escape are [Settings](settings.md#shell).

The shared `Dialog` sets `aria-modal`. `DetailTip`'s pinned card is the remaining React Aria dialog and can omit `aria-modal` on its node: nested dialogs still share keyboard ownership, and the outer shell respects consumed key events and the top visible dialog. Settings sits on the shared dialog layer (the Modal recipe's `z-30`), so the dialog opened last is on top: Settings opened from a Bot editor or its create dialog comes up above it and Escape closes only Settings. Settings leaves Escape and Tab to overlays, listboxes and menus after it in the document, never to a dialog left open underneath. A dialog focuses its close control on opening and restores its trigger after exit. `tests/browser/dialog-motion-browser.mjs` covers keyboard opening, nested Escape and focus, and, on the composed workspace, the two ways a Bot's draft meets another dialog: Add computer over Create Bot, and New computer in Settings beside the sidebar editor.

### Conversation-specific rules

The [message layout contract](message-layout.md#message-layout-contract) is authoritative for presentation and measurement. Working indicators use fixed bounds and event-derived labels; never invent progress. Standalone loading uses `BotLoader` / `LoadingStatus`, not execution-status icons. History, remount and reduced motion do not replay arrival effects. Entry points and test commands are in [CONTRIBUTING.md](https://github.com/Veynode/core/blob/main/CONTRIBUTING.md).
