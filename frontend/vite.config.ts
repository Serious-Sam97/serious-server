import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    // The backend's CSP only allows data: for images; emit fonts as files.
    assetsInlineLimit: (file) => (/\.(woff2?|ttf|otf)$/.test(file) ? false : undefined),
  },
  server: {
    proxy: {
      '/api': {
        // In the dockerized dev setup the backend is another container.
        target: process.env.VITE_PROXY_TARGET ?? 'http://127.0.0.1:8420',
        ws: true,
      },
    },
  },
})
