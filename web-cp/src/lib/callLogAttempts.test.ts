import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 渲染产品链组件，逐次核对日志；不替换网络或伪造浏览器请求。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { RequestAttemptChain } from "./components/CallLogDetails.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "call-attempt-chain.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { RequestAttemptChain } = loaded.exports as { RequestAttemptChain: ComponentType<Record<string, unknown>> };
const attempts = [0, 1, 2].map(index => ({
  event_id: `event-${index}`, occurred_at: "2026-11-16T02:00:00Z", attempt_index: index,
  attempt_kind: ["initial", "retry", "fallback"][index], is_final_attempt: index === 2,
  status_code: index === 2 ? 200 : 503, error_class: index === 2 ? null : "upstream_unavailable",
  error_message: index === 2 ? null : `attempt ${index} full error`,
  charge_micro_cny: index + 1, adjusted_micro_cny: null, cost_micro_cny: 0,
  upstream_account_name: "account", upstream_model: `target-${index === 2 ? 1 : 0}`,
  lines: [], usage: null, cny_per_usd_e4: null, discount_bps: 0,
  stored_event: { event_id: `event-${index}`, event: { custom_observed_field: `observed-${index}` } },
}));
const group = { attempt_count: 3, failover_count: 1, retry_count: 1, charge_micro_cny: "9007199254740993", cost_micro_cny: null, adjusted_micro_cny: "2", complete: true };
function render(extra: Record<string, unknown> = {}): string {
  return renderToStaticMarkup(createElement(RequestAttemptChain, { charge: { ...attempts[2], attempts, request_group: group }, loading: false, error: null, ...extra }));
}
const text = (markup: string) => markup.replace(/<[^>]+>/g, "");
function article(markup: string, eventId: string): string {
  const found = markup.match(new RegExp(`<article[^>]*data-attempt-event="${eventId}"[^>]*>(.*?)</article>`, "s"));
  assert.ok(found, `缺少独立尝试 ${eventId}`);
  return found[1]!;
}

// 漏一条、把所有日志换成代表行、或把两种尝试类型混合，都会失败。
test("管理员请求链逐次显示错误、完整日志与独立尝试类型", () => {
  const markup = render();
  assert.match(markup, /aria-label="故障转移与重试链"/);
  assert.match(text(markup), /已加载 3 \/ 3 次尝试/);
  for (const [index, attempt] of attempts.entries()) {
    const own = article(markup, attempt.event_id);
    assert.match(text(own), new RegExp(`observed-${index}`));
    if (attempt.error_message) assert.ok(text(own).includes(attempt.error_message));
    assert.match(own, new RegExp(`aria-label="尝试 ${index + 1}事件 JSON"`));
  }
  assert.match(text(article(markup, "event-0")), /首次尝试 · 已重试/);
  assert.match(text(article(markup, "event-1")), /同目标重试 · 已重试/);
  assert.match(text(article(markup, "event-2")), /故障转移 · 成功 · 最终尝试/);
});

// 总额超出number精度；未知成本不能由代表尝试的零成本补成免费。
test("请求总额保持精确且汇总未知金额不回退到代表行", () => {
  const content = text(render());
  assert.match(content, /请求总售价¥9,007,199,254\.740993/);
  assert.match(content, /请求总成本未知/);
  assert.match(content, /累计费用调整¥0\.000002/);
  assert.match(content, /故障转移次数1同目标重试次数1/);
});

// 上报到达顺序可晚于尝试顺序，分页累积后的链仍按请求编号排列。
test("迟到的尝试日志按尝试编号归位", () => {
  const markup = render({ charge: { ...attempts[2], attempts: [attempts[2], attempts[0], attempts[1]], request_group: group } });
  const order = [...markup.matchAll(/data-attempt-event="([^"]+)"/g)].map(match => match[1]);
  assert.deepEqual(order, ["event-0", "event-1", "event-2"]);
});

// 后续链不可静默截断；初次加载、加载失败与尚无最终记录分别可见。
test("链分页与错误保留显式继续入口且不把无终态当成功", () => {
  const markup = render({ loading: true, error: "read failed", onMore: () => {} });
  assert.match(markup, /disabled=""[^>]*>重试读取尝试/);
  assert.match(text(markup), /正在读取尝试记录/);
  assert.match(text(markup), /尝试记录读取失败：read failed/);
  assert.match(text(render({ onMore: () => {} })), /加载更多尝试/);
  const partial = render({ charge: { ...attempts[0], attempts: [attempts[0]], request_group: { ...group, complete: false } } });
  assert.match(text(partial), /已加载 1 \/ 3 次尝试/);
  assert.match(text(partial), /尚无最终结果记录/);
  const withoutFirst = render({ charge: { ...attempts[2], attempts: attempts.slice(1), request_group: group } });
  assert.throws(() => article(withoutFirst, "event-0"), /缺少独立尝试/);
});
