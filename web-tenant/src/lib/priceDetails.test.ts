import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import type { Dimension, PriceRate, TierQuote } from "./priceQuote";

// 编译真实展开组件与渲染器，直接检查用户可读的价格矩阵。
const compiled = await build({
  stdin: {
    contents: `import { createElement } from "react";
      import { renderToStaticMarkup } from "react-dom/server";
      import { PriceDetails } from "./PriceDetails.tsx";
      export function render(tiers) { return renderToStaticMarkup(createElement(PriceDetails, { tiers })); }`,
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "price-details-component.ts",
  },
  bundle: true,
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  define: { "import.meta.env.BASE_URL": '"/"' },
  write: false,
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-price-details-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { render } = createRequire(import.meta.url)(entry) as { render: (tiers: Record<string, TierQuote>) => string };

function text(markup: string): string {
  return markup.replace(/<[^>]+>/g, "").replace(/&gt;/g, ">").replace(/&lt;/g, "<");
}

function rate(dimension: Dimension, value: string | null, fallback?: Dimension): PriceRate {
  return { dimension, unit: dimension === "web_search" ? "request" : "token", price_cny_per_million: value, ...(fallback ? { fallback_from: fallback } : {}) };
}

test("一个服务档的多个上下文段按列对齐，项目只列一次且价格不串列", () => {
  const markup = render({
    default: {
      base: [rate("input", "1"), rate("output", "2")],
      long_context: [
        { above_input_tokens: 200_000, rates: [rate("input", "10"), rate("output", "20")] },
        { above_input_tokens: 400_000, rates: [rate("input", "100"), rate("output", "200")] },
      ],
    },
  });
  assert.equal((markup.match(/<table\b/g) ?? []).length, 1, "同一档的价格段必须在一张矩阵中并排比较");
  const inputRows = [...markup.matchAll(/<tr\b[^>]*>([^]*?)<\/tr>/g)].map((match) => match[1]!).filter((row) => /<th\b[^>]*scope="row"[^>]*>输入<\/th>/.test(row));
  assert.equal(inputRows.length, 1);
  assert.deepEqual([...inputRows[0]!.matchAll(/<(?:td|th)\b[^>]*>([^]*?)<\/(?:td|th)>/g)].map((match) => text(match[1]!)), ["输入", "元/百万 token", "¥1.00", "¥10.00", "¥100.00"]);
  assert.match(text(markup), /输入 ≤ 200,000 token/);
  assert.match(text(markup), /输入 > 200,000 token/);
  assert.match(text(markup), /且 ≤ 400,000 token/);
  assert.match(text(markup), /输入 > 400,000 token/);
});

test("任意档位保留十一维、精确小数、零、缺价、回退与按次单位", () => {
  const dimensions: Dimension[] = ["input", "output", "cache_read", "cache_write", "cache_write_1h", "reasoning", "audio_input", "audio_output", "image_input", "image_output", "web_search"];
  const base = dimensions.map((dimension) => rate(dimension, "1"));
  base[0] = rate("input", "9007199254740993.00000000000001");
  base[2] = rate("cache_read", "0");
  base[3] = rate("cache_write", "1", "input");
  base[8] = rate("image_input", null);
  base[10] = rate("web_search", "1250");
  const markup = render({ "custom.fast/v2": { base, long_context: [] } });
  assert.match(text(markup), /custom.fast\/v2/);
  for (const dimension of ["输入", "输出", "缓存读", "缓存写", "1 小时缓存写", "推理", "音频输入", "音频输出", "图像输入", "图像输出", "联网搜索"]) {
    assert.equal((markup.match(new RegExp(`<th\\b[^>]*scope="row"[^>]*>${dimension}</th>`, "g")) ?? []).length, 1, `${dimension}必须完整保留且不重复列出`);
  }
  assert.match(text(markup), /¥9,007,199,254,740,993\.00000000000001/);
  assert.match(text(markup), /缓存读元\/百万 token¥0\.00/);
  assert.match(text(markup), /未提供价格/);
  assert.match(text(markup), /按输入价计费/);
  assert.match(text(markup), /联网搜索元\/千次¥1\.25/);
  assert.match(text(markup), /组织实际单价（已含优惠）/);
  for (const group of ["文本与推理", "缓存", "音频", "图像", "联网"]) assert.ok(text(markup).includes(group));
});

test("标准、批量及自定义服务档同时存在时都可查看各自完整价格", () => {
  const markup = render({
    "custom.fast/v2": { base: [rate("input", "7")], long_context: [{ above_input_tokens: 600_000, rates: [rate("input", "70")] }] },
    batch: { base: [rate("input", "0.5")], long_context: [] },
    default: { base: [rate("input", "1")], long_context: [] },
  });
  assert.equal((markup.match(/<table\b/g) ?? []).length, 3);
  const headings = [...markup.matchAll(/<h4\b[^>]*>([^]*?)<\/h4>/g)].map((match) => text(match[1]!));
  assert.deepEqual(headings, ["标准", "batch", "custom.fast/v2"]);
  for (const amount of ["¥1.00", "¥0.50", "¥7.00", "¥70.00"]) assert.ok(text(markup).includes(amount), amount);
  assert.match(text(markup), /输入 > 600,000 token/);
});
