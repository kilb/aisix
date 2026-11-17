import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 只读使用显式提供的真实后端与管理员页面。
for (const name of ["KEY_LAYOUT_QA_SESSION", "KEY_LAYOUT_QA_OUTPUT", "KEY_LAYOUT_ADMIN_ORIGIN"]) {
  if (!process.env[name]) throw new Error(`${name} is required for admin key layout E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "admin-key-layout-470.spec.ts",
  fullyParallel: true,
  workers: 2,
  retries: 0,
  timeout: 45_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.KEY_LAYOUT_QA_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.KEY_LAYOUT_QA_OUTPUT!, "results.json") }]],
  use: { trace: "off", locale: "zh-CN" },
});
