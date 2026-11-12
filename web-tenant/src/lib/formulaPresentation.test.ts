import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { FormulaBalance, FormulaCall } from "./formulaApi";

/** 只在内存编译时导出私有组件，直接渲染产品代码，不替换网络或复制呈现逻辑。 */
async function component<Props>(file: string, name: string): Promise<ComponentType<Props>> {
  const source = await readFile(new URL(`../${file}`, import.meta.url), "utf8");
  const compiled = await build({
    stdin: {
      contents: `${source}\nexport { ${name} };`,
      resolveDir: fileURLToPath(new URL("../", import.meta.url)),
      sourcefile: file,
      loader: "tsx",
    },
    bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
    loader: { ".css": "empty" },
    define: { "import.meta.env.BASE_URL": '"/"' },
  });
  const compiledModule = { exports: {} as Record<string, ComponentType<Props>> };
  new Function("module", "exports", "require", compiled.outputFiles[0]!.text)(
    compiledModule, compiledModule.exports, createRequire(import.meta.url),
  );
  const result = compiledModule.exports[name];
  assert.ok(result, `${name} must be the rendered product component`);
  return result;
}

const [FeeCalculation, BalanceCalculation] = await Promise.all([
  component<{ call: FormulaCall }>("Usage.tsx", "FeeCalculation"),
  component<{ balance: FormulaBalance }>("Billing.tsx", "BalanceCalculation"),
]);

const textOf = (html: string): string => html.replace(/<[^>]+>/g, "");

test("数量超精确复算范围时解释限制，仍完整显示记录的最终费用", () => {
  const call: FormulaCall = {
    event_id: "presentation-only", occurred_at: "2026-11-11T00:00:00Z", request_id: null,
    model: { id: null, name: "公开模型" }, api_key: { id: "key", label: "", prefix: "" },
    member: null, status: "succeeded", error_class: null, cache_hit: false,
    latency_ms: null, first_token_latency_ms: null, attempt_duration_ms: null,
    charge: {
      status: "charged", amount_micro_cny: 0, adjusted_micro_cny: 0, group: null,
      estimated: false, discount_bps: 0,
      lines: [{ dimension: "input", quantity: Number.MAX_SAFE_INTEGER + 1, rate_cny: "1" }],
      calculation: {
        original_micro_cny: "9007199254740993", adjusted_micro_cny: "0",
        final_micro_cny: "9007199254740993", rounding: "ceil_micro_cny",
      },
    },
  };
  const text = textOf(renderToStaticMarkup(createElement(FeeCalculation, { call })));
  assert.ok(text.includes("最终费用¥9,007,199,254.740993"), text);
  assert.ok(text.includes("计量、折扣不支持精确复算，无法复算"), text);
  assert.ok(!text.includes("历史费率明细未记录"), text);
  assert.ok(!text.includes("按本次最终计价明细复算"), text);
});

test("可用额度溢出时显示说明，不把饱和值画成普通加法结果", () => {
  const balance: FormulaBalance = {
    billing_mode: "prepaid", balance_micro_cny: 0, credit_limit_micro_cny: 0,
    available_micro_cny: 0, exhausted: true, pending_topups: 0,
    calculation: {
      topups_micro_cny: "9223372036854775807", usage_micro_cny: "0", adjustments_micro_cny: "0",
      balance_micro_cny: "9223372036854775807", credit_limit_micro_cny: "1",
      available_micro_cny: "9223372036854775807", available_overflow: true,
      last_ledger_seq: "1", as_of: "2026-11-11T00:00:00Z",
    },
  };
  const overflow = textOf(renderToStaticMarkup(createElement(BalanceCalculation, { balance })));
  assert.ok(overflow.includes("余额加信用额度超出支持范围，可用额度显示上限值"), overflow);
  assert.ok(!overflow.includes(" = 可用额度"), overflow);
  const normal: FormulaBalance = { ...balance, exhausted: false, calculation: {
    ...balance.calculation, topups_micro_cny: "2", balance_micro_cny: "2",
    credit_limit_micro_cny: "3", available_micro_cny: "5", available_overflow: false,
  } };
  const exact = textOf(renderToStaticMarkup(createElement(BalanceCalculation, { balance: normal })));
  assert.ok(exact.includes("余额 ¥0.000002 + 信用额度 ¥0.000003 = 可用额度 ¥0.000005"), exact);
  assert.ok(!exact.includes("超出支持范围"), exact);
});
