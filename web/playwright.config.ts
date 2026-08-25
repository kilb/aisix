import { defineConfig } from "@playwright/test";

/**
 * E2E 打真实后端，不 stub 网络。
 *
 * `e2e/fixture.ts` 会起一个真的 `aisix-console` 进程（指向临时的
 * resources.yaml 和真的 `aisix validate` 二进制）并用 `vite preview`
 * 服务构建产物 —— 也就是说测的是将要部署的那份产物，不是 dev server。
 */
export default defineConfig({
  testDir: "e2e",
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  // 没有 baseURL：fixture 每次起在随机端口上（固定端口会撞上没清干净的
  // 进程），所以用例从 fixture 取实际地址。
  use: { trace: "retain-on-failure" },
  timeout: 30_000,
});
