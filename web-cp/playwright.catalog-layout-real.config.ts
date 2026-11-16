import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real catalog layout E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "catalog-layout-452.spec.ts",
  fullyParallel: true,
  workers: 2,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-artifacts-catalog-layout"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-catalog-layout.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 } },
});
