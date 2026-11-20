import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 仅验收已准备好的真实隔离环境，不启动服务、不替换接口。
for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real call-log E2E`);
}
export default defineConfig({
  testDir: "./e2e-real", testMatch: process.env.CALL_LOG_QA_CANCEL_CASE ? "call-cancel-timing.spec.ts" : "call-log.spec.ts", workers: 1, retries: 0,
  timeout: 40_000, expect: { timeout: 6_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "call-log-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "call-log-playwright.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles", trace: "off", permissions: ["clipboard-read", "clipboard-write"] },
});
