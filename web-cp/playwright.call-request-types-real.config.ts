import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 只消费隔离真环境和网关实际产生的场景，不替换任何网络响应。
for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT", "CALL_TYPES_QA_FIXTURE"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real request-type E2E`);
}
export default defineConfig({
  testDir: "./e2e-real", testMatch: "call-request-types-454.spec.ts", workers: 2, retries: 0,
  timeout: 60_000, expect: { timeout: 8_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "call-types-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "call-types-playwright.json") }]],
  use: { trace: "off", screenshot: "only-on-failure", video: "off" },
});
