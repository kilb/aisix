import assert from "node:assert/strict";
import test from "node:test";
import { directProbeTargets, probeDuration, probeRate, probeMeasurements, probeResultMessage, probeRequestPlan, capabilityChecks, capabilityStatus, capabilityDuration, runDirectProbeBatch, DirectProbeNotSent, type DirectProbeResult, type DirectProbeCapability } from "./directModelProbe.ts";

const available = (id: string): DirectProbeResult => ({
  model_id: id, status: "available", ttft_ms: null, total_duration_ms: 1000,
  http_status: 200, usage: null, output_tokens_per_second: null, error: null, note: null,
  checked_at: "2026-11-11T12:00:00Z", capability_check: {
    status: "not_tested", checks: { json: null, instruction: null, arithmetic: null, sorting: null },
    total_duration_ms: null, usage: null, error: null, note: null,
  }, fidelity: { status: "not_tested", reason: "未验证" },
});

// 若只选可见模型、漏掉 embedding 或对重复 id 发两次请求，此断言必须失败。
test("完整直连集合包含内部目标及 embedding，排除组合模型且同 id 只测一次", () => {
  const models = [
    { id: "manual", doc: { display_name: "手工模型", provider: "openai", model_name: "upstream", provider_key_id: "account" } },
    { id: "hidden", managed_by: "public_models/pub", doc: { model_name: "internal" } },
    { id: "embedding", doc: { embedding: { dimensions: 16 } } },
    { id: "route", doc: { routing: {} } },
    { id: "ensemble", doc: { ensemble: {} } },
    { id: "semantic", doc: { semantic: {} } },
    { id: "manual", doc: { display_name: "重复模型" } },
  ];
  const targets = directProbeTargets(models, [{ id: "account", doc: { display_name: "上游账号" } }]);
  assert.deepEqual(targets.map((target) => target.id), ["manual", "hidden", "embedding"]);
  assert.deepEqual(targets[0], { id: "manual", name: "手工模型", provider: "openai", upstream: "upstream", account: "上游账号" });
  assert.equal(targets[1]?.name, "hidden");
});

test("首字缺失和速率缺失显示未知，报告的零值保留", () => {
  assert.equal(probeDuration(null), "—");
  assert.equal(probeDuration(undefined), "—");
  assert.equal(probeDuration(0), "0 ms");
  assert.equal(probeDuration(1250), "1.25 s");
  assert.equal(probeRate(null), "—");
  assert.equal(probeRate(0), "0 tokens/s");
  assert.equal(probeRate(12.345), "12.35 tokens/s");
});

// 并发上限被移除、重发任一 id 或在首个失败后终止全队列都会使此检查变红。
test("批量探测有界并发，每个模型只发一次，失败后继续其余模型", async () => {
  const targets = Array.from({ length: 7 }, (_, i) => ({ id: String(i), name: String(i), provider: "", upstream: "", account: "" }));
  let active = 0;
  let peak = 0;
  const sent: string[] = [];
  const done: string[] = [];
  await runDirectProbeBatch({
    targets,
    signal: new AbortController().signal,
    probe: async (id) => {
      sent.push(id);
      active++;
      peak = Math.max(peak, active);
      await new Promise((resolve) => setImmediate(resolve));
      active--;
      if (id === "1") throw new Error("服务不可达");
      return available(id);
    },
    onUpdate: (id, state) => { if (state.status !== "running") done.push(`${id}:${state.status}`); },
  });
  assert.equal(peak, 3);
  assert.deepEqual(sent, targets.map((target) => target.id));
  assert.equal(new Set(sent).size, 7);
  assert.equal(done.length, 7);
  assert.ok(done.includes("1:failed"));
});

test("停止后不再发送排队请求，已发请求完成后才结束", async () => {
  const stop = new AbortController();
  const targets = Array.from({ length: 5 }, (_, i) => ({ id: String(i), name: String(i), provider: "", upstream: "", account: "" }));
  const sent: string[] = [];
  const release: (() => void)[] = [];
  let complete = false;
  const batch = runDirectProbeBatch({
    targets,
    signal: stop.signal,
    probe: async (id) => {
      sent.push(id);
      await new Promise<void>((resolve) => release.push(resolve));
      return available(id);
    },
    onUpdate: () => undefined,
  }).then(() => { complete = true; });
  assert.deepEqual(sent, ["0", "1", "2"]);
  stop.abort();
  await Promise.resolve();
  assert.equal(complete, false);
  release.forEach((resolve) => resolve());
  await batch;
  assert.deepEqual(sent, ["0", "1", "2"]);
  assert.equal(complete, true);
});

test("已停止的队列不发送请求，错误详情不透传未知异常文本", async () => {
  const stop = new AbortController();
  stop.abort();
  let calls = 0;
  await runDirectProbeBatch({
    targets: [{ id: "a", name: "a", provider: "", upstream: "", account: "" }],
    signal: stop.signal,
    probe: async (id) => { calls++; return available(id); },
    onUpdate: () => undefined,
  });
  assert.equal(calls, 0);
  const errors: string[] = [];
  await runDirectProbeBatch({
    targets: [{ id: "a", name: "a", provider: "", upstream: "", account: "" }],
    signal: new AbortController().signal,
    probe: async () => { throw new Error("Bearer hidden-secret https://internal.example"); },
    onUpdate: (_id, state) => { if (state.status === "failed") errors.push(state.error ?? ""); },
  });
  assert.equal(errors.length, 1);
  assert.doesNotMatch(errors[0] ?? "", /hidden-secret|internal\.example/);
  assert.match(errors[0] ?? "", /未取得探测结果/);
});

// 控制面拒绝发送不得算成上游失败，也不能继续把余队列压向忙碌接口。
test("控制面忙碌或拒绝发送时保留未测试，并停止后续排队", async () => {
  const sent: string[] = [];
  const updates: string[] = [];
  await runDirectProbeBatch({
    targets: Array.from({ length: 8 }, (_, i) => ({ id: String(i), name: String(i), provider: "", upstream: "", account: "" })),
    signal: new AbortController().signal,
    probe: async (id) => { sent.push(id); throw new DirectProbeNotSent("测试繁忙，未调用上游，请稍后重试。"); },
    onUpdate: (_id, state) => { updates.push(state.status); },
  });
  assert.equal(sent.length, 3);
  assert.equal(updates.filter((status) => status === "not_tested").length, 3);
  assert.equal(updates.filter((status) => status === "failed").length, 0);
});

// 调用前不支持的默认 0 不属于测量；正常结束无正文也不能伪造首字延迟。
test("未实际调用的指标为未知，协议成功无正文保留可用及独立说明", () => {
  const unsupported: DirectProbeResult = { ...available("unsupported"), status: "unsupported", total_duration_ms: 0, output_tokens_per_second: 0 };
  assert.deepEqual(probeMeasurements(unsupported), { ttft: "—", duration: "—", rate: "—" });
  const completed: DirectProbeResult = { ...available("no-text"), note: "协议响应成功，但没有可见正文", usage: { input_tokens: 2, output_tokens: 7 }, output_tokens_per_second: 7 };
  assert.deepEqual(probeMeasurements(completed), { ttft: "—", duration: "1 s", rate: "7 tokens/s" });
  assert.equal(probeResultMessage(completed), "协议响应成功，但没有可见正文");
  assert.deepEqual(probeMeasurements(null), { ttft: "—", duration: "—", rate: "—" });
});

// 默认基础模式不能暗中增加能力请求；开启后声明两次上界并留足客户端等待预算。
test("能力抽测只有显式开启才进入请求体、额外调用预算和 90 秒等待", () => {
  assert.deepEqual(probeRequestPlan(false), { body: {}, timeoutMs: 45000, requestsPerModel: 1 });
  assert.deepEqual(probeRequestPlan(true), { body: { capability_check: true }, timeoutMs: 90000, requestsPerModel: 2 });
});

// false 是客观题未通过，null 是未测；调用失败和超时也不能装成能力未通过。
test("能力抽测逐项区分通过、未通过和未测，错误状态不被算成能力结论", () => {
  const capability: DirectProbeCapability = {
    status: "failed", checks: { json: true, instruction: false, arithmetic: null, sorting: true },
    total_duration_ms: 1234, usage: { input_tokens: 0, output_tokens: null }, error: null, note: "小样本线索",
  };
  assert.deepEqual(capabilityChecks(capability).map((check) => [check.label, check.outcome]), [
    ["严格 JSON", "通过"], ["指令遵循", "未通过"], ["算术", "未测试"], ["排序", "通过"],
  ]);
  assert.equal(capabilityStatus(capability), "未通过");
  assert.equal(capabilityStatus({ ...capability, status: "error" }), "调用失败");
  assert.equal(capabilityStatus({ ...capability, status: "timeout" }), "超时");
  assert.equal(capabilityStatus({ ...capability, status: "not_tested" }), "未测试");
  assert.equal(capabilityStatus({ ...capability, status: "passed" }), "通过");
  assert.equal(capabilityStatus({ ...capability, status: "unsupported" }), "不适用");
  assert.equal(capabilityStatus(null), "未测试");
  assert.equal(capabilityDuration(capability), "1.23 s");
  assert.equal(capabilityDuration({ ...capability, status: "not_tested", total_duration_ms: 0 }), "—");
  assert.equal(capabilityDuration({ ...capability, status: "unsupported", total_duration_ms: 0 }), "—");
});

// 删除历史恢复、误把能力 null 补 false 或把不同轮次拼接时，这些断言必须变红。
test("最近已保存样本恢复独立整轮结果，保留过期与中断状态", async () => {
  const helpers = await import("./directModelProbe.ts");
  assert.equal(typeof helpers.savedProbeRows, "function", "刷新后必须能够从已保存记录恢复结果");
  const target = { id: "a", name: "模型A", provider: "openai", upstream: "a", account: "账号" };
  const result = available("a");
  result.capability_check.checks = { json: true, instruction: false, arithmetic: null, sorting: null };
  const record = { id: "9007199254740993", model_id: "a", provider_key_id: "account", model_version: "3", provider_key_version: "4", target: { name: "模型A", account: "账号" }, actor: { id: "operator", email: "operator@example.test" }, mode: "capability" as const, started_at: result.checked_at, finished_at: result.checked_at, state: "completed" as const, stale: true, result };
  const [row] = helpers.savedProbeRows([target], [record]);
  assert.ok(row);
  assert.equal(row.result, result);
  assert.equal(row.history?.id, "9007199254740993");
  assert.equal(row.history?.stale, true);
  assert.equal(row.result?.capability_check.checks.instruction, false);
  assert.equal(row.result?.capability_check.checks.arithmetic, null);
  assert.equal(helpers.savedProbeRows([target], [{ ...record, state: "interrupted", result: null }])[0]?.status, "not_tested");
  assert.equal(helpers.savedProbeRows([target], [])[0]?.result, null);
});
