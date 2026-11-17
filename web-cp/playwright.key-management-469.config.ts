import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";
for (const field of ["KEY_MANAGEMENT_QA_SESSION", "KEY_MANAGEMENT_QA_OUTPUT"]) if (!process.env[field]) throw new Error(`${field} is required`);
export default defineConfig({
  testDir: "./e2e-real", testMatch: "key-management-469.spec.ts", fullyParallel: true, workers: 3, retries: 0, timeout: 90_000,
  expect: { timeout: 8_000 }, outputDir: resolve(process.env.KEY_MANAGEMENT_QA_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.KEY_MANAGEMENT_QA_OUTPUT!, "playwright.json") }]],
  use: { trace: "off", timezoneId: "America/Los_Angeles", locale: "zh-CN" },
});
