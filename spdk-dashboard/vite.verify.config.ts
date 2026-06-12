import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Temporary config for verifying against a live cluster:
// proxies /api to the port-forwarded spdk-dashboard backend.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:52352',
    },
  },
})
