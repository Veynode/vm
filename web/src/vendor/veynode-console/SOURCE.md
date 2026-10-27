# Computer stickers, vendored from core

The containers' and images' icons are core's computer icons (ds 电脑图标, 贴纸), copied unchanged from [Veynode/core](https://github.com/Veynode/core) at commit `219ab4f56`. The paths below mirror core, so a re-sync is a straight copy of the same files. Don't edit them here; change them in core and copy them over.

| Here | Core |
|---|---|
| `components/ui/Sticker.tsx`, `components/ui/NumberField.tsx` | `packages/console/src/components/ui/` |
| `components/computer/computer-stickers.tsx`, `computer-icon.ts`, `ComputerIcon.module.css` | `packages/console/src/components/computer/` |
| `hooks/useResolvedTheme.ts` | `packages/console/src/hooks/useResolvedTheme.ts` |
| `lib/page-transition.ts` | `packages/console/src/lib/page-transition.ts` |
| `shared/avatar.ts` | `packages/shared/src/media/avatar.ts` |

`lib/page-transition.ts` is the phone's page motion; its `pageMotion` reads core's addresses, so `web/src/page-nav.ts` decides vm's own push and pop and calls its `pageTransition`, and `web/src/styles.css` carries the swipe-back and page-motion styles from core's `packages/console/src/styles/base.css`. It imports `@veynode/ui/boardui/swipe-back`, aliased to `veynode-ui/boardui/overlays/swipe-back.tsx`.

The copied files import `@veynode/shared/avatar`; `web/vite.config.ts` and `web/tsconfig.json` alias it to `shared/avatar.ts`, and `@console/*` to this folder. `i18n/provider.tsx` is not core's: it stands in for console's `useI18n` with the English strings the copied files ask for. Core's `ComputerIcon.tsx` and its picker are not copied (they draw Bot faces and uploads): `web/src/container-icon.tsx` draws the sticker the same way, and `web/src/icon-dialog.tsx` is the picker without the upload.

Core is licensed AGPL-3.0-only.
