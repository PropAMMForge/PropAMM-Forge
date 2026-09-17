import { resolve } from 'node:path'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// GitHub Pages serves a project site under `/<repo>/`, not at the root. The
// workflow passes that prefix in `BASE_PATH`; locally it stays `/`. Vite exposes
// the same value to the app as `import.meta.env.BASE_URL`, which is where the
// router takes its basename from — one source for both.
const base = process.env.BASE_PATH ?? '/'

export default defineConfig({
  base,
  plugins: [react()],
  resolve: {
    alias: { '@': resolve(import.meta.dirname, 'src') },
  },
})
