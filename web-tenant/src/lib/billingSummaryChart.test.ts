import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import type { LedgerSummaryRow } from "./api";

// 直接调用账户页实际交给图表的数据投影，不复制排序或金额格式化逻辑。
const compiled = await build({
  stdin: {
    contents: 'export { ledgerSummaryChartPoints } from "./Billing.tsx";',
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "billing-summary-chart.ts",
  },
  bundle: true,
  loader: { ".css": "empty" },
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  define: { "import.meta.env.BASE_URL": '"/"' },
  write: false,
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-billing-chart-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { ledgerSummaryChartPoints } = createRequire(import.meta.url)(entry) as {
  ledgerSummaryChartPoints: (rows: readonly LedgerSummaryRow[], group: "day" | "month") => {
    label: string;
    value: number | null;
    displayValue: string;
  }[];
};

test("两日扣费从1升至9时图表按时间向右增长，表格原顺序保持不变", () => {
  const rows: LedgerSummaryRow[] = [
    { period_start: "2026-11-11T00:00:00Z", usage_micro_cny: 9_000_000, topups_micro_cny: 0, adjustments_micro_cny: 0, closing_balance_micro_cny: 90_000_000 },
    { period_start: "2026-11-10T00:00:00Z", usage_micro_cny: 1_000_000, topups_micro_cny: 0, adjustments_micro_cny: 0, closing_balance_micro_cny: 99_000_000 },
  ];
  const tableRows = structuredClone(rows);
  const points = ledgerSummaryChartPoints(rows, "day");
  assert.deepEqual(points.map((point) => point.label), ["2026-11-10", "2026-11-11"]);
  assert.deepEqual(points.map((point) => point.displayValue), ["¥1.00", "¥9.00"]);
  assert.notEqual(points[0]!.value, null);
  assert.notEqual(points[1]!.value, null);
  assert.ok(points[0]!.value! > 0 && points[0]!.value! < points[1]!.value!, "时间靠后的9元柱必须高于较早的1元柱");
  assert.deepEqual(rows, tableRows, "图表投影不能改变表格最新在前的顺序");
});
