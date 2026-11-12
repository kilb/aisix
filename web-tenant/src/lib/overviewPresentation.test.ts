import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { FormulaBalance } from "./formulaApi";

const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: {
    contents: 'export { OverviewBudgets } from "./Org.tsx";',
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "overview-budget-component.ts",
  },
  bundle: true,
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  define: { "import.meta.env.BASE_URL": '"/"' },
  write: false,
  plugins: [{
    name: "single-react-runtime",
    setup(builder) {
      builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true }));
    },
  }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-overview-budget-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { OverviewBudgets } = require(entry) as { OverviewBudgets: ComponentType<{ budgets: []; error: null; balance: FormulaBalance; balanceError: null; showBalance: true; onRetry: () => void }> };

test("预付费账户的非零信用额度也参与可用额度，不被文案排除", () => {
  const balance: FormulaBalance = {
    billing_mode: "prepaid", balance_micro_cny: 1000000, credit_limit_micro_cny: 5000000, available_micro_cny: 6000000, exhausted: false, pending_topups: 0,
    calculation: { topups_micro_cny: "1000000", usage_micro_cny: "0", adjustments_micro_cny: "0", balance_micro_cny: "1000000", credit_limit_micro_cny: "5000000", available_micro_cny: "6000000", available_overflow: false, last_ledger_seq: "1", as_of: "2026-11-11T00:00:00Z" },
  };
  const text = renderToStaticMarkup(createElement(OverviewBudgets, { budgets: [], error: null, balance, balanceError: null, showBalance: true, onRetry: () => {} })).replace(/<[^>]+>/g, "");
  assert.match(text, /账户可用额度¥6\.00/);
  assert.match(text, /可用额度为余额与信用额度之和/);
  assert.doesNotMatch(text, /预付费：可用额度为账户余额/);
});
