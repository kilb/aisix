import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";
if (!process.env.PRICING_DIMENSIONS_QA_SESSION || !process.env.PRICING_DIMENSIONS_QA_OUTPUT) throw new Error("Explicit isolated pricing QA session and output required");
export default defineConfig({ testDir: "./e2e-real", testMatch: "pricing-dimensions-483.spec.ts", workers: 1, retries: 0, timeout: 120_000, outputDir: resolve(process.env.PRICING_DIMENSIONS_QA_OUTPUT, "playwright-artifacts"), reporter: [["list"]], use: { actionTimeout: 15_000, navigationTimeout: 30_000, viewport: { width: 1440, height: 1000 }, trace: "retain-on-failure" } });
