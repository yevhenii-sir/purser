import { createHash } from "node:crypto"
import path from "node:path"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"
import { defineConfig, type Plugin } from "vite"
import { viteSingleFile } from "vite-plugin-singlefile"

// SpacetimeDB server the dev server talks to: `npm run dev`, then open
// http://localhost:5173/v1/database/<db>/route/admin
const target = process.env.SPACETIME_URL || "http://127.0.0.1:3000"
const api = { target, changeOrigin: true }

/**
 * Content-Security-Policy of the page with the hash of every inline script, so `script-src` needs no
 * 'unsafe-inline'. Runs after vite-plugin-singlefile has inlined the bundle and hashes the script text
 * exactly as it is in the emitted HTML (the browser hashes the UTF-8 bytes between the tags).
 * Module build: writes the header value to csp.txt next to index.html (the module sends it with the
 * page). Demo build (a static page): adds it as a <meta> tag, which cannot carry frame-ancestors.
 */
function contentSecurityPolicy(demo: boolean): Plugin {
  const policy = (hashes: string[], meta: boolean) =>
    [
      "default-src 'none'",
      `script-src ${hashes.map((h) => `'sha256-${h}'`).join(" ")}`,
      "style-src 'unsafe-inline'",
      `connect-src ${meta ? "'none'" : "'self'"}`,
      "img-src 'self' data:",
      "font-src data:",
      "base-uri 'none'",
      "form-action 'none'",
      ...(meta ? [] : ["frame-ancestors 'none'"]),
    ].join("; ")
  return {
    name: "purser-csp",
    apply: "build",
    enforce: "post",
    generateBundle: {
      order: "post",
      handler(_options, bundle) {
        const page = bundle["index.html"]
        if (!page || page.type !== "asset") return this.error("index.html is not in the bundle")
        const html = String(page.source)
        if (/<script\b[^>]*\bsrc=/i.test(html)) return this.error("external <script src> in index.html: the CSP allows inline scripts by hash only")
        // The HTML parser turns CR LF and CR into LF before the browser hashes a script.
        const hashes = [...html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/gi)].map((m) =>
          createHash("sha256").update(m[1].replace(/\r\n?/g, "\n"), "utf8").digest("base64"))
        if (!hashes.length) return this.error("no inline <script> in index.html")
        if (demo) {
          const tag = `<meta http-equiv="Content-Security-Policy" content="${policy(hashes, true)}" />`
          if (!/<meta charset="UTF-8" \/>/.test(html)) return this.error("no <meta charset> to put the CSP after")
          page.source = html.replace(/<meta charset="UTF-8" \/>/, (m) => `${m}\n    ${tag}`)
        } else {
          this.emitFile({ type: "asset", fileName: "csp.txt", source: policy(hashes, false) })
        }
      },
    },
  }
}

// The admin page is served by the billing module itself (`GET /route/admin`), embedded into the
// wasm with `include_str!`. Everything is inlined into one index.html written next to the module.
// `--mode demo` (`npm run build:demo`): the read-only demo with the sample data of demo/snapshot.json,
// written to dist-demo/index.html instead of the module.
export default defineConfig(({ mode }) => ({
  plugins: [react(), tailwindcss(), viteSingleFile(), contentSecurityPolicy(mode === "demo")],
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
