import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real overview acceptance`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "overview-loading.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 45_000,
  expect: { timeout: 7_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "overview-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "overview-playwright.json") }]],
  use: { trace: "off", screenshot: "off", video: "off", viewport: { width: 1440, height: 1000 } },
});
