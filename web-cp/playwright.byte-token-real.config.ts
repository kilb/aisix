import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 只验收已准备的真实隔离记录；不启动栈、不制造事件或推理流量。
for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT", "BYTE_TOKEN_QA_CASES"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real byte-token E2E`);
}
export default defineConfig({
  testDir: "./e2e-real", testMatch: "byte-token-estimate.spec.ts", workers: 2, retries: 0,
  timeout: 40_000, expect: { timeout: 6_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "byte-token-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "byte-token-playwright.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles", trace: "off" },
});
