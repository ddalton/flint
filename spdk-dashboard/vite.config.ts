/// <reference types="vitest/config" />
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  build: {
    rollupOptions: {
      output: {
        // Keep the graph/chart vendors out of the entry chunk; they load
        // with the tabs that use them (Phase 4 code-split).
        manualChunks: {
          'vendor-reactflow': ['reactflow'],
          'vendor-recharts': ['recharts'],
        },
      },
    },
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    restoreMocks: true,
  },
  server: {
    // Same-origin API in dev: point at a real backend (a port-forwarded
    // dashboard pod on 8080) instead of relying on CORS or in-bundle mocks:
    //   kubectl -n flint-system port-forward deploy/spdk-dashboard 8080:8080
    // Override the target when local 8080 is taken:
    //   VITE_API_PROXY_TARGET=http://localhost:18080 npm run dev
    proxy: {
      '/api': {
        target: process.env.VITE_API_PROXY_TARGET ?? 'http://localhost:8080',
        changeOrigin: true,
      },
    },
  },
})
