import { defineConfig } from "@playwright/test";
import { resolve } from "node:path";
if (!process.env.UNIFIED_QA_OUTPUT) throw new Error("UNIFIED_QA_OUTPUT is required");
export default defineConfig({testDir:"./e2e-real",testMatch:"frontend-unified-456.spec.ts",workers:1,retries:0,timeout:90_000,expect:{timeout:10_000},outputDir:resolve(process.env.UNIFIED_QA_OUTPUT,"artifacts"),reporter:[["list"],["json",{outputFile:resolve(process.env.UNIFIED_QA_OUTPUT,"results.json")}]],use:{viewport:{width:1440,height:1000},trace:"off",screenshot:"off",video:"off",timezoneId:"America/Los_Angeles"}});
