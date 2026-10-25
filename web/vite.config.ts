import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'

export default defineConfig({
  server: { host: '127.0.0.1', port: 3417, strictPort: true },
  // Bundle server dependencies so dist/ runs without node_modules.
  ssr: { noExternal: true },
  plugins: [tanstackStart(), react()],
})
