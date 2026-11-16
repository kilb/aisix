import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real table layout E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "table-widths.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "table-widths-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "table-widths-results.json") }]],
  use: { trace: "off" },
});
