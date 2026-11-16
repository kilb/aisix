import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";

for (const name of ["CONSOLE_TEMPLATE_QA_SESSION", "CONSOLE_TEMPLATE_QA_OUTPUT"]) {
  if (!process.env[name]) throw new Error(`${name} is required for real console template QA`);
}
export default defineConfig({
  testDir: "./e2e-real",
  testMatch: ["console-template.spec.ts", "console-template-management.spec.ts"],
  workers: 2,
  retries: 0,
  timeout: 60_000,
  expect: { timeout: 7_000 },
  outputDir: resolve(process.env.CONSOLE_TEMPLATE_QA_OUTPUT!, "playwright-artifacts"),
  reporter: [["list"], ["json", { outputFile: resolve(process.env.CONSOLE_TEMPLATE_QA_OUTPUT!, "playwright-console-template.json") }]],
  use: { trace: "off" },
});
