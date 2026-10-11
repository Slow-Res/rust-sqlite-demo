import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// The backend address the dev server forwards `/api` to. Override it to run the
// backend elsewhere, e.g. `API_PROXY_TARGET=http://127.0.0.1:4100 npm run dev`.
const apiProxyTarget = process.env.API_PROXY_TARGET ?? 'http://127.0.0.1:4000'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: { '/api': apiProxyTarget },
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    globals: true,
    restoreMocks: true,
  },
})
