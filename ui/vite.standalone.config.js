import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { resolve } from 'node:path'

// Standalone build: the React console plus the WebAssembly engine, no server.
//
// Not a single file this time — the .wasm is emitted alongside index.html so
// the browser can compile it by streaming rather than decoding a base64 blob
// out of the HTML. Two files, any static host.
export default defineConfig({
  plugins: [react()],
  base: './',
  build: {
    target: 'es2022',
    outDir: 'dist-standalone',
    emptyOutDir: true,
    reportCompressedSize: false,
    rollupOptions: {
      input: resolve(process.cwd(), 'index-standalone.html'),
      output: {
        entryFileNames: 'app.js',
        assetFileNames: '[name][extname]',
      },
    },
  },
  resolve: {
    alias: { '@glider/wasm': resolve(process.cwd(), '../ts/dist/index.js') },
  },
})
