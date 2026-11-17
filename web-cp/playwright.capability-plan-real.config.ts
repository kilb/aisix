import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["CAPABILITY_PLAN_QA_SESSION", "CAPABILITY_PLAN_QA_OUTPUT", "CAPABILITY_PLAN_QA_URL"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real capability plan E2E`);
}
export default defineConfig({
  testDir: "./e2e-real", testMatch: "capability-plan-472.spec.ts", workers: 1, retries: 0,
  timeout: 45_000, expect: { timeout: 7_000 },
  outputDir: resolve(process.env.CAPABILITY_PLAN_QA_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CAPABILITY_PLAN_QA_OUTPUT!, "results.json") }]],
  use: { trace: "off" },
});
