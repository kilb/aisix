import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 真实隔离验收必须显式指定已恢复的世界；缺少环境直接失败，不回退到mock。
for (const name of ["REFINED_QA_SESSION", "REFINED_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real refined UI E2E`);
}
export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "refined.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 35_000,
  expect: { timeout: 5_000 },
  outputDir: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-artifacts-tenant"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.REFINED_QA_OUTPUT!, "playwright-tenant.json") }]],
  use: { trace: "off" },
});
