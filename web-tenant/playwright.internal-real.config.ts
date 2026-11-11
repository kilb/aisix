import { defineConfig } from "@playwright/test";
import realConfig from "./playwright.real.config";

// 内部legacy协议验收单独运行；沿用真实provider配置的强制检查。
// 所选上游必须实际支持 /v1/completions，不支持时保留真实失败。
export default defineConfig(realConfig, {
  testIgnore: [],
  testMatch: "**/organization-internal-streams.spec.ts",
});
