import assert from "node:assert/strict";
import { test } from "node:test";
import { routingModeOf, routingPolicy, routingValueOf, selectRoutingMode } from "./routingModes.ts";
import { draftOf, routingBody, statusesOf } from "./publicRouting.ts";

test("only explicit API key ID affinity qualifies as the recommended mode", () => {
  assert.equal(routingModeOf({ strategy: "consistent_hash", affinity: "api_key" }), "api_key");
  for (const affinity of ["default", "custom"] as const) {
    assert.equal(routingModeOf({ strategy: "consistent_hash", affinity }), "preserve");
  }
  assert.equal(routingModeOf({ strategy: "least_cost", affinity: "default" }), "preserve");
  assert.equal(routingModeOf({ strategy: "future_strategy", affinity: "custom" }), "preserve");
});

test("three explicit choices serialize the existing canonical strategies without losing other settings", () => {
  // 错写策略、推荐项未覆盖亲和来源、切离哈希未清理来源都会让断言失败。
  const stored = {
    strategy: "consistent_hash", hash_on: [{ type: "header", name: "x-session" }],
    max_fallbacks: 1, future_knob: { keep: true },
  };
  const cases = [
    { mode: "api_key", want: { strategy: "consistent_hash", hash_on: [{ type: "api_key" }], max_fallbacks: 1, future_knob: { keep: true } } },
    { mode: "round_robin", want: { max_fallbacks: 1, future_knob: { keep: true } } },
    { mode: "failover", want: { strategy: "failover", max_fallbacks: 1, future_knob: { keep: true } } },
  ] as const;
  for (const { mode, want } of cases) {
    const draft = selectRoutingMode(draftOf(stored, "round_robin"), mode);
    assert.deepEqual(routingBody(draft, stored, "round_robin", statusesOf(draft.statuses)), want);
  }
});

test("preserve mode keeps custom affinity, unrecognized strategies and unknown parameters exactly", () => {
  for (const strategy of ["consistent_hash", "least_latency", "future_strategy"]) {
    const stored = { strategy, hash_on: [{ type: "header", name: "x-tenant" }, { type: "api_key" }], future_knob: [7, 2] };
    const draft = selectRoutingMode(draftOf(stored, "round_robin"), "preserve");
    assert.deepEqual(routingBody(draft, stored, "round_robin", statusesOf(draft.statuses)), stored);
  }
});

test("an absent hash chain stays absent until the recommendation is explicitly selected", () => {
  const stored = { strategy: "consistent_hash", retries: 2 };
  const draft = draftOf(stored, "round_robin");
  assert.deepEqual(routingBody(draft, stored, "round_robin", []), stored);
  const selected = selectRoutingMode(draft, "api_key");
  assert.deepEqual(routingBody(selected, stored, "round_robin", []), {
    strategy: "consistent_hash", retries: 2, hash_on: [{ type: "api_key" }],
  });
  assert.deepEqual(draft, draftOf(stored, "round_robin"));
});

test("summary distinguishes actual saved affinity and strategies from the recommendation", () => {
  assert.deepEqual(routingValueOf({ strategy: "consistent_hash", hash_on: [{ type: "api_key" }] }, "failover"), {
    strategy: "consistent_hash", affinity: "api_key",
  });
  assert.equal(routingValueOf({}, "failover").strategy, "failover");
  assert.equal(routingPolicy({ strategy: "consistent_hash", affinity: "api_key" }).affinityLabel, "API key ID 亲和");
  assert.equal(routingPolicy({ strategy: "consistent_hash", affinity: "default" }).affinityLabel, "网关默认：请求头 → API key ID");
  assert.equal(routingPolicy({ strategy: "consistent_hash", affinity: "custom" }).affinityLabel, "自定义亲和来源链");
  assert.equal(routingPolicy({ strategy: "failover", affinity: "default" }).title, "同级固定顺序");
  assert.equal(routingPolicy({ strategy: "future_strategy", affinity: "default" }).title, "future_strategy");
});

const SESSION_HEADERS = ["x-claude-code-session-id", "session-id", "thread-id"].map((name) => ({ type: "header", name }));

test("会话固定显式写入官方会话头链与禁止切换，不回退密钥", () => {
  const initial = draftOf({ retries: 0, max_fallbacks: 0, future_knob: "keep" }, "round_robin");
  const selected = selectRoutingMode(initial, "session");
  const body = routingBody(selected, { future_knob: "keep" }, "round_robin", []);
  assert.deepEqual(body, { strategy: "consistent_hash", hash_on: SESSION_HEADERS, hash_failover: false, retries: 0, max_fallbacks: 0, future_knob: "keep" });
  assert.equal(routingModeOf(routingValueOf(body, "round_robin")), "session");
  assert.equal(initial.strategy, "round_robin");
});

test("只将完整会话头链且禁止切换的配置认作会话固定", () => {
  for (const body of [
    { strategy: "consistent_hash", hash_on: SESSION_HEADERS },
    { strategy: "consistent_hash", hash_on: SESSION_HEADERS, hash_failover: true },
    { strategy: "consistent_hash", hash_on: [...SESSION_HEADERS, { type: "api_key" }], hash_failover: false },
    { strategy: "consistent_hash", hash_on: [...SESSION_HEADERS].reverse(), hash_failover: false },
  ]) assert.equal(routingModeOf(routingValueOf(body, "round_robin")), "preserve");
});

test("离开会话固定预设恢复既有切换行为，自定义禁止切换原样保存", () => {
  const stored = { strategy: "consistent_hash", hash_on: SESSION_HEADERS, hash_failover: false, retries: 0 };
  const draft = draftOf(stored, "round_robin");
  assert.deepEqual(routingBody(draft, stored, "round_robin", []), stored);
  const apiKey = routingBody(selectRoutingMode(draft, "api_key"), stored, "round_robin", []);
  assert.deepEqual(apiKey, { strategy: "consistent_hash", hash_on: [{ type: "api_key" }], retries: 0 });
  assert.deepEqual(routingBody(selectRoutingMode(draft, "failover"), stored, "round_robin", []), { strategy: "failover", retries: 0 });
  const custom = { strategy: "consistent_hash", hash_on: [{ type: "header", name: "x-existing" }], hash_failover: false, future_knob: true };
  const customDraft = draftOf(custom, "round_robin");
  customDraft.retries = "2";
  assert.deepEqual(routingBody(customDraft, custom, "round_robin", []), { ...custom, retries: 2 });
});
