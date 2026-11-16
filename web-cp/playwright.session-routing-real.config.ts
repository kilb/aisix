import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real session routing E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "session-routing.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-artifacts-session-routing"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-session-routing.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 } },
});
