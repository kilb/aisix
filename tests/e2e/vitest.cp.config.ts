import { defineConfig } from "vitest/config";

// The cross-plane cases: a real gateway in managed mode against the real
// control plane, kine and Postgres (`scripts/cp-dev-stack.mjs`). A separate
// config, like the real-provider smoke's, so the main suite needs no Postgres
// and never discovers a `*.cp.ts`.
export default defineConfig({
  test: {
    include: ["src/cp-cases/**/*.cp.ts"],
    globalSetup: "./src/harness/cp-global-setup.ts",
    pool: "forks",
    maxWorkers: 2,
    testTimeout: 60_000,
    hookTimeout: 150_000,
    globals: false,
  },
});
