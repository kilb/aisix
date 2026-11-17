import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["ADMIN_SECTION_QA_STATE", "ADMIN_SECTION_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real admin section navigation E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "admin-section-navigation.spec.ts",
  fullyParallel: true,
  workers: 3,
  retries: 0,
  timeout: 120_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.ADMIN_SECTION_QA_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.ADMIN_SECTION_QA_OUTPUT!, "results.json") }]],
  use: { trace: "off", screenshot: "off", video: "off", viewport: { width: 1440, height: 1000 } },
});
