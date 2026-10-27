import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'

export default defineConfig(({ command }) => ({
  // The shadcn/ui components and the Veynode Soft theme, vendored from core (src/vendor/veynode-ui/SOURCE.md),
  // and the console's computer stickers (src/vendor/veynode-console/SOURCE.md). The `@veynode/*`
  // entries are the package paths the copied files import.
  resolve: {
    alias: {
      '@ui': fileURLToPath(new URL('./src/vendor/veynode-ui', import.meta.url)),
      '@veynode/ui/icons': fileURLToPath(new URL('./src/vendor/veynode-ui/icons/index.tsx', import.meta.url)),
      '@veynode/ui/boardui/swipe-back': fileURLToPath(new URL('./src/vendor/veynode-ui/boardui/overlays/swipe-back.tsx', import.meta.url)),
      '@console': fileURLToPath(new URL('./src/vendor/veynode-console', import.meta.url)),
      '@veynode/shared/avatar': fileURLToPath(new URL('./src/vendor/veynode-console/shared/avatar.ts', import.meta.url)),
    },
  },
  server: { host: '127.0.0.1', port: 3417, strictPort: true },
  // Bundle server dependencies so dist/ runs without node_modules. Build only:
  // in dev the module runner can't evaluate CommonJS dependencies such as react.
  ssr: command === 'build' ? { noExternal: true } : {},
  plugins: [tailwindcss(), tanstackStart(), react()],
}))
