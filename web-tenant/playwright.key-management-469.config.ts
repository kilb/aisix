import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["KEY_MANAGEMENT_QA_SESSION", "KEY_MANAGEMENT_QA_OUTPUT", "KEY_MANAGEMENT_TENANT_URL"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real key management E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "key-management-469.spec.ts",
  fullyParallel: true,
  workers: 2,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 5_000 },
  outputDir: resolve(process.env.KEY_MANAGEMENT_QA_OUTPUT!, "playwright-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.KEY_MANAGEMENT_QA_OUTPUT!, "playwright.json") }]],
  use: { trace: "off", timezoneId: "America/Los_Angeles", locale: "zh-CN" },
});
