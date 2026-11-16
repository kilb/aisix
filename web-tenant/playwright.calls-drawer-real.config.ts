import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real call drawer acceptance`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "calls-drawer.spec.ts",
  fullyParallel: true,
  workers: 3,
  retries: 0,
  timeout: 45_000,
  expect: { timeout: 7_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-artifacts-calls-drawer"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-calls-drawer.json") }]],
  use: { trace: "off", screenshot: "off", video: "off", viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles" },
});
