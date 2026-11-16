import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT", "PRICING_DRILLDOWN_FIXTURE"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real pricing drilldown E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "pricing-status-drilldown.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 60_000,
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-pricing-drilldown"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "pricing-drilldown.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 } },
});
