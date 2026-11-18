import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "admin-pagination-483.spec.ts",
  workers: 1,
  timeout: 90_000,
  outputDir: resolve(process.env.ADMIN_PAGINATION_QA_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.ADMIN_PAGINATION_QA_OUTPUT!, "playwright.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 }, locale: "zh-CN" },
});
