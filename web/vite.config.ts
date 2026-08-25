import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

/** 本机控制台的地址。E2E 起在 8091，`npm run dev` 默认 8090。 */
const CONSOLE = process.env.CONSOLE_ORIGIN ?? "http://127.0.0.1:8090";
const DEV_PROXY = {
  "/api": { target: CONSOLE, changeOrigin: true },
  "/healthz": { target: CONSOLE, changeOrigin: true },
};

// 单一 JS/CSS 产物：这份前端由 Rust 二进制内嵌后原样吐出，没有
// HTTP/2 push、没有 CDN，拆包只会变成串行的额外往返。
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks: undefined,
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/app.js",
        assetFileNames: "assets/app.[ext]",
      },
    },
  },
  // 开发与 E2E 都把接口代理到本地控制台。生产不经过这里 —— nginx 直接
  // 托管 dist 并把 /api 反代到控制台，所以这段只影响本机。
  //
  // E2E 用 `vite preview` 服务的是**构建产物**，也就是将要部署的那一份；
  // dev server 会多一层转换，测它证明不了产物没问题。
  server: { proxy: DEV_PROXY },
  preview: { proxy: DEV_PROXY },
});
