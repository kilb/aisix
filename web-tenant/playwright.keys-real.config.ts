import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real key UI E2E`);
}

export default defineConfig({
  testDir: "./e2e-keys-real",
  testMatch: "keys.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 45_000,
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-key-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-keys.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles" },
});
