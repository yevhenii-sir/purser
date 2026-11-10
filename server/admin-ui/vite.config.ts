import path from "node:path"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"
import { defineConfig } from "vite"
import { viteSingleFile } from "vite-plugin-singlefile"

// SpacetimeDB server the dev server talks to: `npm run dev`, then open
// http://localhost:5173/v1/database/<db>/route/admin
const target = process.env.SPACETIME_URL || "http://127.0.0.1:3000"
const api = { target, changeOrigin: true }

// The admin page is served by the billing module itself (`GET /route/admin`), embedded into the
// wasm with `include_str!`. Everything is inlined into one index.html written next to the module.
// `--mode demo` (`npm run build:demo`): the read-only demo with the sample data of demo/snapshot.json,
// written to dist-demo/index.html instead of the module.
export default defineConfig(({ mode }) => ({
  plugins: [react(), tailwindcss(), viteSingleFile()],
  define: { __DEMO__: JSON.stringify(mode === "demo") },
  resolve: {
    alias: { "@": path.resolve(import.meta.dirname, "./src") },
  },
  server: {
    proxy: {
      // Everything the page calls, except the page itself (served by Vite in dev).
      "^/v1/database/[^/]+/(sql|call|logs)": api,
      "^/v1/database/[^/]+/route/(?!admin)": api,
      "^/v1/identity": api,
    },
  },
  build: {
    outDir: mode === "demo" ? "dist-demo" : "../spacetimedb/admin",
    emptyOutDir: mode === "demo",
    chunkSizeWarningLimit: mode === "demo" ? 12000 : 2000,
  },
}))
