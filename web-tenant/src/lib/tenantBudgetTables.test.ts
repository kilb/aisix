import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import type { Budget, TenantKey } from "./api";
import type { FormulaCall } from "./formulaApi";

// 内存中导出实际私有组件；不替换业务数据源，不写临时文件。
async function renderer(file: string, body: string) {
  const source = await readFile(new URL(`../${file}`, import.meta.url), "utf8");
  const compiled = await build({
    stdin: {
      contents: `${source}\nimport { createElement as element } from "react";\nimport { renderToStaticMarkup } from "react-dom/server";\n${body}`,
      resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: file, loader: "tsx",
    },
    bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
    loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  });
  const module = { exports: {} };
  new Function("module", "exports", "require", compiled.outputFiles[0]!.text)(module, module.exports, createRequire(import.meta.url));
  return module.exports;
}

const [budget, fee] = await Promise.all([
  renderer("Budgets.tsx", `
    export function figures(keys, quotas) { return renderToStaticMarkup(element(KeyFigures, { keys, rows: [], quotas, onVisibleKeys() {} })); }
    export function quotas(rows) { return renderToStaticMarkup(element(QuotaTable, { label: "成员额度", rows, empty: "没有额度", role: "owner", parentsOf: () => [], onChanged() {} })); }
  `),
  renderer("Usage.tsx", 'export function calculation(call) { return renderToStaticMarkup(element(FeeCalculation, { call })); }'),
]) as [{ figures: (keys: TenantKey[], quotas: Record<string, unknown>) => string; quotas: (rows: Budget[]) => string }, { calculation: (call: FormulaCall) => string }];

const keys = Array.from({ length: 61 }, (_, index) => ({ id: `key-${index + 1}`, label: `公开密钥-${String(index + 1).padStart(3, "0")}` })) as TenantKey[];

test("密钥实际可用按父记录显示首25条，未知、零和不限互不替代", () => {
  const html = budget.figures(keys, {
    "key-1": { effective_remaining_micro_cny: 0, binding_level: "key" },
    "key-3": { effective_remaining_micro_cny: null, binding_level: null },
  });
  assert.equal((html.match(/<tr[ >]/g) ?? []).length, 26, "25条父记录加1条表头");
  assert.match(html, /公开密钥-025/);
  assert.doesNotMatch(html, /公开密钥-026/);
  assert.match(html, /共 61 条/);
  const rows = [...html.matchAll(/<tbody>([^]*?)<\/tbody>/g)][0]![1]!.match(/<tr[^]*?<\/tr>/g)!;
  const text = rows.map((row) => row.replace(/<[^>]+>/g, ""));
  assert.equal(text[0], "公开密钥-001¥0.00密钥");
  assert.equal(text[1], "公开密钥-002——");
  assert.equal(text[2], "公开密钥-003不限—");
});

test("周期预算显示后端真实UTC结算起止换算的北京08点，分页不丢总数", () => {
  const rows = keys.map((key, index): Budget => ({
    id: `budget-${index}`, scope: "member", target: { id: key.id, label: key.label }, target_live: true,
    set_by: "operator", kind: "periodic", period: "day", period_start: "2026-11-11T00:00:00Z", counted_from: null,
    created_at: "2026-11-10T00:00:00Z", limit_micro_cny: 1000000, spent_micro_cny: 100000, remaining_micro_cny: 900000, exhausted: false,
  }));
  const html = budget.quotas(rows);
  assert.equal((html.match(/<tr[ >]/g) ?? []).length, 26);
  assert.match(html, /2026-11-11 08:00:00 至 2026-11-12 08:00:00（不含结束时刻）/);
  assert.match(html, /共 61 条/);
  assert.doesNotMatch(html, /公开密钥-026/);
});

test("成功调用的估算标签也解释token来源，费用仍按记录精确展示", () => {
  const call = {
    status: "succeeded", charge: { status: "charged", estimated: true, lines: [], discount_bps: 0,
      calculation: { original_micro_cny: "9007199254740993", adjusted_micro_cny: "0", final_micro_cny: "9007199254740993", rounding: "ceil_micro_cny" } },
  } as unknown as FormulaCall;
  const html = fee.calculation(call);
  assert.match(html, /上游未完整返回用量，部分 token 由网关估算，费用按该用量计费/);
  assert.match(html, /最终费用[^]*?¥9,007,199,254\.740993（估算）/);
  assert.doesNotMatch(fee.calculation({ ...call, charge: { ...call.charge, estimated: false } }), /部分 token 由网关估算/);
});
