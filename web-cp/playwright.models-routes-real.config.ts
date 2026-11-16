import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real model editor E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "models-routes-redesign.spec.ts",
  fullyParallel: true,
  workers: 3,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-artifacts-models-routes"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-models-routes.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 } },
});
