import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import type { FormulaCall } from "./formulaApi";

// 内存中导出实际状态组件，验证最终文本与指示灯状态。
const source = await readFile(new URL("../Usage.tsx", import.meta.url), "utf8");
const compiled = await build({
  stdin: {
    contents: `${source}
      import { createElement as element } from "react";
      import { renderToStaticMarkup } from "react-dom/server";
      export function lamp(call) { return renderToStaticMarkup(element(CallStatusLamp, { call })); }`,
    resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "Usage.tsx", loader: "tsx",
  },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
});
const module = { exports: {} };
new Function("module", "exports", "require", compiled.outputFiles[0]!.text)(module, module.exports, createRequire(import.meta.url));
const { lamp } = module.exports as { lamp: (call: FormulaCall) => string };

test("未知结果显示中性说明，不误报失败或重试", () => {
  const html = lamp({ status: "unknown", status_code: null, error_class: null } as unknown as FormulaCall);
  assert.equal(html.replace(/<[^>]+>/g, ""), "结果未知");
  assert.match(html, /data-lamp="pending"/);
  assert.doesNotMatch(html, /失败|已重试|data-lamp="rejected"/);
});

test("已知调用结果保留成功、重试和公开失败原因", () => {
  for (const [status, error, text, state] of [
    ["succeeded", null, "成功", "applied"],
    ["retried", null, "已重试", "pending"],
    ["failed", "upstream_unavailable", "失败（上游暂不可用）", "rejected"],
  ]) {
    const html = lamp({ status, error_class: error } as FormulaCall);
    assert.equal(html.replace(/<[^>]+>/g, ""), text);
    assert.ok(html.includes(`data-lamp="${state}"`));
  }
});
