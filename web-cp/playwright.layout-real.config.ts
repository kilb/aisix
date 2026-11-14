import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["CP_LAYOUT_SESSION", "CP_LAYOUT_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real control-plane layout E2E`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "layout.spec.ts",
  workers: 3,
  retries: 0,
  timeout: 120_000,
  expect: { timeout: 8_000 },
  outputDir: resolve(process.env.CP_LAYOUT_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CP_LAYOUT_OUTPUT!, "results.json") }]],
  use: { trace: "off", viewport: { width: 1440, height: 1000 } },
});
