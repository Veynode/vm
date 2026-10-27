# Veynode design system

> Interface decisions for designers and AI developers. Applies to the console both hosts mount (the node's page and the account's web app), the machine page and the desktop app's windows.
> Direction: quiet, clear, reliable, and under the user's control. Build on BoardUI and Apple's principles of hierarchy, spacing, and consistency.

## 1. How to use this document

Read the standing decisions below, then only the file that owns what you are changing (table at the end). Load only relevant components, tokens, and rules; avoid pulling entire component libraries, compiled translations, or historical evaluations into context.

| Decision                                                      | Authoritative location                                                                                                |
| ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| Scope, execution rules, required verification                 | [Root AGENTS.md](https://github.com/Veynode/core/blob/main/AGENTS.md) and applicable directory instructions                                                     |
| Visual style, scales, hierarchy, copy, interaction principles | This document and its files                                                                                           |
| Component APIs, theme variables, current behavior             | Current source; the files below name the entry points                                                                 |
| Conversation, work, execution identity, control               | [Conversation and Work contract](https://github.com/Veynode/core/blob/main/packages/controller-core/src/conversations/AGENTS.md#conversation-and-work-contract) |
| Style ownership (which stylesheet holds what)                 | [Style guide](https://github.com/Veynode/core/blob/main/packages/console/src/styles/README.md)                                                                  |
| Translation keys, PO catalogs, compilation                    | [i18n guide](https://github.com/Veynode/core/blob/main/packages/console/src/i18n/README.md)                                                                     |
| Setup, commands, feature locations and verification commands  | [CONTRIBUTING.md](https://github.com/Veynode/core/blob/main/CONTRIBUTING.md)                                                                                    |

**One answer per decision, said in one place.** Each rule lives in exactly one file; every other file links to it instead of restating it. Sections 4–8 give one utility per colour role, one recipe per text role, one value per spacing context, one size per control and one primitive per recurring pattern. When a table names a value, use that value; do not pick a neighbour. "Owned by" means the value lives inside a shared component and feature code must not restate it. `pnpm check:design` enforces what it can mechanically (see [Enforcement](design-system/motion-copy-accessibility.md#enforcement)); the rest is review.

**Must** denotes a delivery requirement. **Current** describes verifiable implementation behavior, not a recommendation to expand it. Explicit user requests determine scope. Specific authorization, dialog, and message contracts take precedence over general visual recommendations. Existing code does not exempt a change from mandatory rules.

Fix and verify gaps within the current task; do not rewrite unrelated pages. Reuse components first. When extending a shared component, inspect its other consumers and regression scope. Do not describe an unimplemented goal as an existing capability.

Decision order: identify the user action and data state → find the matching entry in the [pattern catalog](design-system/components.md#pattern-catalog) → choose tokens and sizes from sections 4–6 → place it in the [shell](design-system/shell.md#workspace-shell) → cover states, languages, themes and narrow screens → run applicable acceptance checks. Choose a page, inline detail, or Modal based on the task; component convenience is not a reason to make every feature a dialog.

## Standing decisions

The owner's decisions that cut across every screen. Each file below applies them; none may contradict them.

- **One line, one height.** A row is one line at a fixed height: 48px in a list or a Settings group, led by a bare 20px icon or the thing's own mark; 56px for a conversation; 44px in a sidebar. A state never adds a line: it changes a badge, a value or the control in place ([A state change keeps the height](design-system/components.md#a-state-change-keeps-the-height)).
- **States are badges and dots.** A state is a `StateTag` badge or a dot, with its word or accessible description; never a sentence on a line of its own and never colour alone.
- **Say a thing once.** One wording per fact, one home per action, no restating what a neighbour shows ([No needless words](design-system/motion-copy-accessibility.md#no-needless-words)).
- **Occasional actions in ··· and right click.** A row has at most one direct control; the rest are in its ··· `RowMenu`, which a right click on the row opens too.
- **Flat structure.** Groups of rows, not cards in cards; no nested boxes, side bars or shadows in the page; at most two levels under a Settings page.
- **Fixed-height heads.** Every head (sidebar, pane, page, dialog, window) has one height in every state; a notice about that area goes into its head as a badge or a capsule, never as a line of its own under it ([Heads](design-system/shell.md#heads)).
- **No needless words.** Labels, values and controls, not explanatory sentences; an explanation goes behind an ⓘ.

## Where each part lives

Code comments cite sections by name (for example DESIGN.md "Dialog anatomy"); find the heading in the file that owns it.

| File                                                                         | Owns                                                                                                                                                                                                                                                                                                   | Does not own                                                                |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------- |
| [Design foundations](design-system/foundations.md)                           | 2. Product hierarchy; 3. Visual direction and materials; 4. Color; 5. Typography; 6. Scales: spacing, sizes, radii, elevation, icons; 7. Breakpoints and responsiveness                                                                                                                                | Where things stand on screen (shell), how components use the scales         |
| [Workspace shell](design-system/shell.md)                                    | The columns and panes, every head (sidebar, chat, side pane, Settings band, page head, dialog, computer window) and how they line up, the sidebar's rows and foot, compact and narrow layouts, the Mac and Windows desktop app (glass, title bars, traffic lights, drag regions, full screen, windows) | The transcript's rows, Settings pages                                       |
| [Components](design-system/components.md)                                    | 8. Controls, tags, rows and lists, menus, tooltips, toasts, dialogs (Dialog anatomy), loading/empty/failed states, Error placement, the Pattern catalog                                                                                                                                                | Dialog and menu motion lifecycle (BoardUI contract)                         |
| [BoardUI contract](design-system/boardui.md)                                 | BoardUI provenance, the controls taken from it, the mandatory dialog and menu motion lifecycle                                                                                                                                                                                                         | Visual values (foundations), feature behaviour                              |
| [Interactions: conversation and status](design-system/interactions.md)       | 9. Conversation, composer, finder, AI status, approvals, the computer window's behaviour, the Bot and Group editors                                                                                                                                                                                    | Transcript measurement and row presentation (message layout)                |
| [Settings](design-system/settings.md)                                        | The Settings spec: shell behaviour, addresses, rows and controls, information architecture, every page                                                                                                                                                                                                 | Shared row primitives' look (components), the title band's geometry (shell) |
| [Message layout contract](design-system/message-layout.md)                   | The transcript: registration, measurement, scroll ownership, the seven row styles and cards, the gallery, budgets                                                                                                                                                                                      | Composer behaviour (interactions)                                           |
| [Motion, copy and accessibility](design-system/motion-copy-accessibility.md) | 10. Motion tokens and the motion table; 11. Copy and i18n; 12. Accessibility and acceptance; Enforcement; 13. References and maintenance                                                                                                                                                               | —                                                                           |

## Open owner decisions

No decision is open.
