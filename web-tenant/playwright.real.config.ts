import { defineConfig } from "@playwright/test";

// 真实付费验收独立运行，默认CI的e2e目录不会收集本套测试。
// CI可注入API_BASE/API_KEY/MODEL；本地证据runner可使用只读SSH配置。
const direct = [process.env.ORG_E2E_PROVIDER_API_BASE, process.env.ORG_E2E_PROVIDER_API_KEY];
const ssh = [process.env.ORG_E2E_PROVIDER_SSH_HOST, process.env.ORG_E2E_PROVIDER_KINE_KEY];
if (direct.some(Boolean) ? !direct.every(Boolean) : !ssh.every(Boolean)) {
  throw new Error("Real E2E requires ORG_E2E_PROVIDER_API_BASE + ORG_E2E_PROVIDER_API_KEY (optional ORG_E2E_PROVIDER_MODEL), or the authorized read-only SSH host + kine key path; no mock/skip fallback is allowed.");
}

export default defineConfig({
  testDir: "./e2e-real",
  // 客户验收只收集门户合同；内部legacy协议由单独入口完整实跑。
  testIgnore: "**/organization-internal-streams.spec.ts",
  workers: 1,
  timeout: 60_000,
  use: { trace: "retain-on-failure" },
});
