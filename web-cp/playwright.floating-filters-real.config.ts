import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["FLOATING_FILTERS_QA_SESSION", "FLOATING_FILTERS_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real floating filter E2E`);
}
export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "floating-filters-458.spec.ts",
  workers: 3,
  retries: 0,
  timeout: 30_000,
  expect: { timeout: 5_000 },
  outputDir: resolve(process.env.FLOATING_FILTERS_QA_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.FLOATING_FILTERS_QA_OUTPUT!, "results.json") }]],
  use: { trace: "off" },
});
