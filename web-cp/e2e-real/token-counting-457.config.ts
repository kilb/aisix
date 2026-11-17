import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

if (!process.env.TOKEN_COUNTING_QA_OUTPUT) throw new Error("TOKEN_COUNTING_QA_OUTPUT is required");
export default defineConfig({
  testDir: ".",
  testMatch: "token-counting-457.spec.ts",
  fullyParallel: true,
  workers: 2,
  retries: 0,
  timeout: 180_000,
  expect: { timeout: 10_000 },
  outputDir: resolve(process.env.TOKEN_COUNTING_QA_OUTPUT, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.TOKEN_COUNTING_QA_OUTPUT, "results.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, trace: "off", screenshot: "off", video: "off" },
});
