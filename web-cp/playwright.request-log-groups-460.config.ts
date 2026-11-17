import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 仅接受显式绑定的真实隔离世界，不启动替身后端或补造用量。
for (const name of ["REQUEST_GROUPS_QA_SESSION", "REQUEST_GROUPS_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real request-log grouping E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "request-log-groups-460.spec.ts",
  fullyParallel: true,
  workers: 4,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.REQUEST_GROUPS_QA_OUTPUT!, "playwright-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REQUEST_GROUPS_QA_OUTPUT!, "playwright.json") }]],
  use: { trace: "off", timezoneId: "America/Los_Angeles", locale: "zh-CN" },
});
