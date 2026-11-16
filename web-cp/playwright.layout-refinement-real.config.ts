import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// session 提供真实 CP 预览地址、登录与真实保留事件的 layout-oracle.json。
// 不启动替身服务，不替换网络；证据输出必须显式放在仓库外。
if (!process.env.LAYOUT_QA_SESSION || !process.env.LAYOUT_QA_OUTPUT) {
  throw new Error("LAYOUT_QA_SESSION and LAYOUT_QA_OUTPUT are required");
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "layout-refinement-452.spec.ts",
  fullyParallel: true,
  workers: 6,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  outputDir: resolve(process.env.LAYOUT_QA_OUTPUT, "artifacts"),
  reporter: [
    ["list"],
    ["json", { outputFile: resolve(process.env.LAYOUT_QA_OUTPUT, "results.json") }],
  ],
  use: {
    viewport: { width: 1440, height: 1000 },
    trace: "off",
    video: "off",
    screenshot: "off",
  },
});
