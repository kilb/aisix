import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

/** The control plane's browser listener. The e2e stack passes its own. */
const CP = process.env.CP_ORIGIN ?? "http://127.0.0.1:8090";
const PROXY = { "/api": { target: CP, changeOrigin: true } };

export default defineConfig({
  plugins: [react(), tailwindcss()],
  // The portal's public path. Development and e2e serve it at the root; a
  // deployment that mounts it under a prefix builds with that prefix, and the
  // API prefix follows it (src/lib/api.ts).
  base: process.env.TENANT_BASE ?? "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // 字体外置以满足 font-src 'self'；其它资产继续采用 Vite 默认内联策略。
    assetsInlineLimit: (filePath) => /\.(?:woff2?|ttf|otf)$/i.test(filePath) ? false : undefined,
    rollupOptions: {
      output: {
        // Content hashes in the file names are the only cache invalidation
        // there is: assets are served with a long max-age, so a fixed name
        // would keep returning visitors on the old build however fresh
        // `index.html` is. (Same as web-portal/vite.config.ts.)
        entryFileNames: "assets/[name].[hash].js",
        chunkFileNames: "assets/[name].[hash].js",
        assetFileNames: "assets/[name].[hash].[ext]",
      },
    },
  },
  // Development and e2e proxy the API to a local control plane; production
  // serves `dist/` behind a reverse proxy that does the same. e2e runs
  // `vite preview`, which serves the built artifact that ships.
  server: { proxy: PROXY },
  preview: { proxy: PROXY },
});
