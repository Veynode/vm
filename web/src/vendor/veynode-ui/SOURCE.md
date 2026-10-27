# Veynode UI, vendored from core

The web UI's components and theme are a copy of `packages/ui` (`@veynode/ui`) in [Veynode/core](https://github.com/Veynode/core) at commit `219ab4f56`: shadcn/ui components on Base UI drawn in Veynode Soft, core's Settings shell, groups and rows, and the Soft theme as Tailwind v4 tokens. The paths below mirror `packages/ui/src/`, so a re-sync is a straight copy of the same files. Don't edit them here; change them in core and copy them over.

| Here | Core `packages/ui/src/` |
|---|---|
| `components/ui/` alert, badge, button, checkbox, close-button, dialog, dropdown-menu, icon-button, input, label, menu-rows, popover, select, select-field, switch, tabs, textarea, toast, tooltip | `components/ui/` |
| `settings/` field, info-tip, inline-error, list-row, notice, progress-ring, recipes, row-menu, settings-action-row, settings-link-row, settings-page, settings-row, settings-section, sidebar-head, state-fit, state-tag | `settings/` |
| `boardui/icons/`, `boardui/overlays/`, `boardui/utils/cx.ts`, `boardui/utils/use-icon-swap.ts` | `boardui/` |
| `brand/logo.tsx`, `brand/logo-marks.ts` | `brand/` |
| `icons/`, `lib/utils.ts` | `icons/`, `lib/` |
| `styles/boardui-theme.css`, `styles/boardui-typography.css`, `styles/base.css` | `styles/` |

The copied files import `@veynode/ui/icons`; `web/vite.config.ts` and `web/tsconfig.json` alias it to `icons/index.tsx`, and `@ui/*` to this folder.

Local changes, all mechanical:

- `brand/logo.tsx` names the product `VeynodeVM`.
- `styles/base.css` imports the fonts' default entry (`@fontsource-variable/geist`) instead of `/wght.css`, and drops core-only blocks: the onboarding step and digit-roll animations and the phone `SidebarGroup` hairline.

Core switches dark mode with `.dark` on `<html>`; `web/src/routes/__root.tsx` sets it from `prefers-color-scheme`.

Provenance: [shadcn-sources.json](shadcn-sources.json) (core's records for the copied components), [SHADCN-NOTICE.md](SHADCN-NOTICE.md) (MIT) and [BOARDUI-NOTICE.md](BOARDUI-NOTICE.md). Core is licensed AGPL-3.0-only.
