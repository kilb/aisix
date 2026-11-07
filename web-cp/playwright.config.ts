import { defineConfig } from "@playwright/test";

// From web-portal/playwright.config.ts.
export default defineConfig({
  testDir: "./e2e",
  // One worker: each spec file starts a whole stack (Postgres databases,
  // kine, the control plane, a gateway, vite preview) on real ports.
  workers: 1,
  timeout: 60_000,
  use: { trace: "retain-on-failure" },
});
