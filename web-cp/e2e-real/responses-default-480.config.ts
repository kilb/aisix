import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

if (!process.env.RESPONSES_DEFAULT_QA_OUTPUT || !process.env.RESPONSES_DEFAULT_QA_STATE) {
  throw new Error("RESPONSES_DEFAULT_QA_OUTPUT and RESPONSES_DEFAULT_QA_STATE are required");
}

export default defineConfig({
  testDir: ".",
  testMatch: "responses-default-480.spec.ts",
  fullyParallel: true,
  workers: 1,
  retries: 0,
  timeout: 180_000,
  expect: { timeout: 10_000 },
  outputDir: resolve(process.env.RESPONSES_DEFAULT_QA_OUTPUT, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.RESPONSES_DEFAULT_QA_OUTPUT, "results.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, trace: "off", screenshot: "only-on-failure", video: "off" },
});
