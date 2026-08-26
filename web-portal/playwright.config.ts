import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  // 一个 worker：夹具会起真进程、占端口、写临时库，并行会互相踩。
  workers: 1,
  timeout: 60_000,
  use: { trace: "retain-on-failure" },
});
