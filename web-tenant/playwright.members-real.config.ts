import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 成员验收只连接显式提供的真实隔离世界，不启动服务或回退到替身。
for (const name of ["MEMBERS_QA_SESSION", "MEMBERS_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real member management E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "member-management.spec.ts",
  fullyParallel: true,
  workers: 2,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 7_000 },
  outputDir: resolve(process.env.MEMBERS_QA_OUTPUT!, "playwright-artifacts-members"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.MEMBERS_QA_OUTPUT!, "playwright-members.json") }]],
  // 测试会输入真实凭据；任何失败都不录制密码输入界面。
  use: { trace: "off", screenshot: "off", video: "off", viewport: { width: 1440, height: 1000 } },
});
