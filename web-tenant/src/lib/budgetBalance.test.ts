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
import type { Balance } from "./api";
import type { FormulaBalance } from "./formulaApi";

type BalanceWire = Balance & {
  calculation?: Partial<Pick<FormulaBalance["calculation"], "available_micro_cny" | "credit_limit_micro_cny">>;
};

// 直接编译并渲染产品组件，不复制其金额选择或格式化实现，也不替换网络。
const compiled = await build({
  stdin: {
    contents: 'export { BudgetAccountAmounts } from "./BudgetAccountAmounts.tsx";',
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "budget-balance-component.ts",
  },
  bundle: true,
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  write: false,
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-budget-balance-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { BudgetAccountAmounts } = createRequire(import.meta.url)(entry) as {
  BudgetAccountAmounts: ComponentType<{ balance: BalanceWire }>;
};

function display(balance: BalanceWire): string {
  return renderToStaticMarkup(createElement(BudgetAccountAmounts, { balance })).replace(/<[^>]+>/g, "");
}

const base: Balance = {
  billing_mode: "postpaid", balance_micro_cny: 0, available_micro_cny: 0,
  credit_limit_micro_cny: 0, exhausted: false, pending_topups: 0,
};

test("预算账户组件从精确wire字符串展示超过JS安全整数的金额", () => {
  const wire: BalanceWire = JSON.parse('{"billing_mode":"postpaid","balance_micro_cny":-2,"available_micro_cny":9007199254740993,"credit_limit_micro_cny":9007199254740995,"exhausted":false,"pending_topups":0,"calculation":{"available_micro_cny":"9007199254740993","credit_limit_micro_cny":"9007199254740995"}}');
  assert.notEqual(BigInt(wire.available_micro_cny).toString(), wire.calculation!.available_micro_cny);
  assert.equal(display(wire), "账户可用额度¥9,007,199,254.740993信用额度¥9,007,199,254.740995");
});

test("旧控制面缺精确字段时仅回退安全整数，零值仍明确显示", () => {
  assert.equal(display({ ...base, credit_limit_micro_cny: 12345 }), "账户可用额度¥0.00信用额度¥0.012345");
  assert.equal(display({ ...base, available_micro_cny: -Number.MAX_SAFE_INTEGER, credit_limit_micro_cny: Number.MAX_SAFE_INTEGER }), "账户可用额度-¥9,007,199,254.740991信用额度¥9,007,199,254.740991");
});

test("旧控制面的不安全金额显示未知，不能把舍入值当精确金额", () => {
  assert.equal(display({ ...base, available_micro_cny: Number.MAX_SAFE_INTEGER + 1, credit_limit_micro_cny: -(Number.MAX_SAFE_INTEGER + 1) }), "账户可用额度未知信用额度未知");
});

test("存在精确对象但缺金额时显示未知，不从legacy零值伪造金额", () => {
  assert.equal(display({ ...base, calculation: {} }), "账户可用额度未知信用额度未知");
  assert.equal(display({ ...base, calculation: { available_micro_cny: "0.5", credit_limit_micro_cny: "invalid" } }), "账户可用额度未知信用额度未知");
});

test("精确零值和负值优先于legacy不安全值，不因零值被当成缺失", () => {
  assert.equal(display({ ...base, available_micro_cny: Number.MAX_SAFE_INTEGER + 1, credit_limit_micro_cny: Number.MAX_SAFE_INTEGER + 1, calculation: { available_micro_cny: "-9007199254740993", credit_limit_micro_cny: "0" } }), "账户可用额度-¥9,007,199,254.740993信用额度¥0.00");
});
