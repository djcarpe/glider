import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { viteSingleFile } from 'vite-plugin-singlefile'

// One self-contained index.html: JS, CSS and all assets inlined. That is what
// lets the Rust side embed the whole console with a single include_str! and
// keeps `glider serve` a single binary with no static file routing.
export default defineConfig({
  plugins: [react(), viteSingleFile()],
  base: './',
  build: {
    target: 'es2020',
    assetsInlineLimit: 100000000,
    chunkSizeWarningLimit: 100000,
    cssCodeSplit: false,
    reportCompressedSize: false,
    rollupOptions: { output: { inlineDynamicImports: true } },
  },
  server: {
    // `npm run dev` against a live glider: proxy the API to the real server so
    // the UI can be developed with hot reload instead of rebuild-and-embed.
    proxy: {
      '/api': 'http://127.0.0.1:7878',
      '/query': 'http://127.0.0.1:7878',
    },
  },
})
