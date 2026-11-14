import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real usage monitoring E2E`);
}
export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "usage-monitor.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 40_000,
  expect: { timeout: 6_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "usage-monitor-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "usage-monitor-playwright.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles", trace: "off" },
});
