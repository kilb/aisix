import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";
if (!process.env.CALL_CARDS_QA_STATE || !process.env.CALL_CARDS_QA_OUTPUT) throw new Error("Real owned stack state and evidence output are required");
export default defineConfig({
  testDir: ".", testMatch: "calls-filter-cards-481.spec.ts", fullyParallel: true, workers: 2, retries: 0,
  timeout: 120_000, expect: { timeout: 10_000 },
  outputDir: resolve(process.env.CALL_CARDS_QA_OUTPUT, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CALL_CARDS_QA_OUTPUT, "results.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, screenshot: "only-on-failure", trace: "off", video: "off" },
});
