import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 只使用统一恢复的真实环境；不启动服务，也不准备业务数据。
for (const name of ["CONSOLE_TEMPLATE_QA_SESSION", "CONSOLE_TEMPLATE_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real console template QA`);
}
export default defineConfig({
  testDir: "./e2e-real", testMatch: "console-template.spec.ts", workers: 2, retries: 0,
  timeout: 180_000, expect: { timeout: 10_000 },
  outputDir: resolve(process.env.CONSOLE_TEMPLATE_QA_OUTPUT!, "playwright-artifacts-admin"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CONSOLE_TEMPLATE_QA_OUTPUT!, "playwright-console-template-admin.json") }]],
  use: { viewport: { width: 1440, height: 1000 }, timezoneId: "America/Los_Angeles", actionTimeout: 12_000, navigationTimeout: 25_000, trace: "off" },
});
