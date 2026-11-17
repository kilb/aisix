import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

// 只读取明确指定的真实隔离后端，产物留在工作树之外。
for (const name of ["CARD_PALETTE_SESSION", "CARD_PALETTE_ORIGIN", "CARD_PALETTE_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required`);
}

export default defineConfig({
  testDir: "./e2e-real",
  testMatch: "card-palette-471.spec.ts",
  workers: 1,
  retries: 0,
  timeout: 120_000,
  expect: { timeout: 10_000 },
  outputDir: resolve(process.env.CARD_PALETTE_OUTPUT!, "artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CARD_PALETTE_OUTPUT!, "results.json") }]],
  use: { locale: "zh-CN", timezoneId: "America/Los_Angeles", trace: "off" },
});
