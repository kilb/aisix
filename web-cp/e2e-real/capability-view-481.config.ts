import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

if (!process.env.CAPABILITY_UI_QA_SESSION || !process.env.CAPABILITY_UI_QA_OUTPUT) throw new Error("An isolated real capability session and evidence directory are required");
export default defineConfig({
  testDir: ".",
  testMatch: "capability-view-481.spec.ts",
  workers: 2,
  fullyParallel: true,
  retries: 0,
  timeout: 35_000,
  outputDir: resolve(process.env.CAPABILITY_UI_QA_OUTPUT, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CAPABILITY_UI_QA_OUTPUT, "results.json") }]],
  use: { launchOptions: { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE }, trace: "off" },
});
