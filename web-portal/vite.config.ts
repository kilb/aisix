import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 门户是独立前端，与 web/（管理控制台）分离 —— 设计文档 §5.1「分进程不分角色」
// 的前端一半：门户的构建产物里根本没有配置编辑的代码。
const target = process.env.PORTAL_ORIGIN ?? "http://127.0.0.1:8091";

export default defineConfig({
  plugins: [react()],
  // 生产上门户挂在 /portal/（控制台在根路径占了 /api/，证书又没有通配，
  // 起不了子域名）。开发与 e2e 保持根路径。
  base: process.env.PORTAL_BASE ?? "/",
  // 内容散列是唯一的缓存失效手段：固定文件名配长缓存等于永不更新。
  build: {
    rollupOptions: {
      output: {
        entryFileNames: "assets/[name].[hash].js",
        chunkFileNames: "assets/[name].[hash].js",
        assetFileNames: "assets/[name].[hash].[ext]",
      },
    },
  },
  server: { proxy: { "/api": target } },
  preview: { proxy: { "/api": target } },
});
