import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { BaselineRevision, BaselineRow, Proposal, RateCard } from "./api.ts";

// 渲染真实确认表单，不替换 React 状态、控件或网络响应；页面交互另由真实后端 E2E 验证。
const require = createRequire(import.meta.url);
const source = await readFile(new URL("../tabs/pricing/PricingImport.tsx", import.meta.url), "utf8");
const compiled = await build({
  stdin: {
    contents: `${source}\nexport { ConfirmForm, CatalogueUpdates }; export { DirectModelMonitoring } from "../../components/DirectModelMonitoring.tsx";`,
    resolveDir: fileURLToPath(new URL("../tabs/pricing/", import.meta.url)), sourcefile: "import-tier-warnings.tsx", loader: "tsx",
  },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { ConfirmForm, CatalogueUpdates, DirectModelMonitoring } = loaded.exports as Record<string, ComponentType<Record<string, unknown>>>;
const standard: RateCard = { default: { base: { input: "4", output: "20" } } };

function revision(model: string, card: RateCard): BaselineRevision {
  return { revision_id: 1, engine_version: 1, model_id: model, effective_from: "2026-11-10T00:00:00Z", card, source_kind: "manual", source: {}, created_by: null, created_at: "2026-11-10T00:00:00Z" };
}

function proposal(model: string, current: BaselineRevision | null): Proposal {
  return {
    id: "proposal", batch_id: "batch", provider: "provider", model: "upstream-model", effective_from: null,
    card: standard, flags: [], dropped: [], pricing_url: null, prices_checked: null, suggested_model_id: model,
    status: "pending", decided_by: null, decided_at: null, reject_reason: null, baseline_revision_id: null,
    current_baseline: current, catalogue_entry: null, same_as_current: false,
  };
}

function render(value: Proposal, rows: BaselineRow[] = []): string {
  return renderToStaticMarkup(createElement(ConfirmForm!, { batchId: "batch", proposal: value, baselineRows: rows, models: [], onDecided: async () => undefined }));
}

// 删除确认前提示会使第一项失败；当前卡上的 fast 不能因对照表只读标准档而被隐藏。
test("legacy import confirmation names removed fast tier before the existing attestation", () => {
  const markup = render(proposal("a", revision("a", { ...standard, fast: { base: { input: "8", output: "40" } } })));
  const text = markup.replace(/<[^>]+>/g, "");
  assert.match(text, /移除档位「fast」/);
  assert.match(text, /整卡替换后，这些档位的请求将无法计价/);
  assert.ok(text.indexOf("移除档位") < text.indexOf("已对照官方价格页核对"));
  assert.match(markup, /<button[^>]*type="submit"[^>]*disabled=""/);
});

// 若沿用提议匹配到的其他模型价格，选择目标 B 会错误报告 A 的 fast，而漏掉 B 的 priority。
test("legacy import warning compares the selected model baseline and never another model", () => {
  const currentA = revision("a", { ...standard, fast: { base: { input: "8", output: "40" } } });
  const currentB = revision("b", { ...standard, priority: { base: { input: "12", output: "60" } } });
  const text = render(proposal("b", currentA), [{ model_id: "b", name: "B", current: currentB, upcoming: [], catalogue_update: null }]).replace(/<[^>]+>/g, "");
  assert.match(text, /移除档位「priority」/);
  assert.doesNotMatch(text, /移除档位「fast」/);
  assert.doesNotMatch(render(proposal("b", currentA)), /移除档位/);
});

test("legacy import leaves unchanged or unknown current tiers without a deletion warning", () => {
  assert.doesNotMatch(render(proposal("a", revision("a", standard))), /移除档位/);
  assert.doesNotMatch(render(proposal("a", null)), /移除档位/);
  assert.doesNotMatch(render(proposal("a", revision("a", { default: { base: {} } }))), /移除档位/);
});

test("catalogue import update also names tiers removed now or by its scheduled card", () => {
  const fast = { ...standard, fast: { base: { input: "8", output: "40" } } };
  const row: BaselineRow = {
    model_id: "a", name: "A", current: revision("a", fast), upcoming: [],
    catalogue_update: { catalogue_version: "v1", entry_key: "provider/model", entries: [{ effective_from: null, card: standard }] },
  };
  const renderUpdate = (value: BaselineRow) => renderToStaticMarkup(createElement(CatalogueUpdates!, { canEdit: true, rows: [value], onSaved: async () => undefined })).replace(/<[^>]+>/g, "");
  assert.match(renderUpdate(row), /移除档位「fast」/);
  assert.match(renderUpdate({ ...row, catalogue_update: { ...row.catalogue_update!, entries: [{ effective_from: null, card: fast }, { effective_from: "2030-11-20T00:00:00Z", card: standard }] } }), /2030-11-20 08:00:00.*移除档位「fast」/);
});

test("upstream monitoring explains accepted tasks and object management exclusion", () => {
  const text = renderToStaticMarkup(createElement(DirectModelMonitoring!)).replace(/<[^>]+>/g, "");
  assert.match(text, /后台任务.*受理.*查询.*取消.*对象管理/);
  assert.match(text, /最终结果.*计入/);
  assert.match(text, /包含失败重试/);
});
