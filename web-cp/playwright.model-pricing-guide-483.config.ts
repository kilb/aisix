import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";
if (!process.env.KEY_MINTER_QA_SESSION || !process.env.KEY_MINTER_QA_OUTPUT) throw new Error("Explicit isolated QA session and output required");
export default defineConfig({ testDir: "./e2e-real", testMatch: "model-pricing-guide-483.spec.ts", workers: 1, retries: 0, timeout: 60_000, outputDir: resolve(process.env.KEY_MINTER_QA_OUTPUT, "admin-artifacts"), reporter: [["list"]], use: { viewport: { width: 1440, height: 1000 }, trace: "retain-on-failure" } });
