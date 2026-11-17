import assert from "node:assert/strict";
import { test } from "node:test";
import * as counting from "./tokenCounting.ts";
import { tokenCountBody, tokenCountSources, tokenCountProblems, selectTokenCountSource } from "./tokenCounting.ts";
import type { ResourceView, TokenCounting } from "./api.ts";

const source = (id: string, doc: Record<string, unknown>, managed_by: string | null = null): ResourceView<Record<string, unknown>> => ({
  id, doc, managed_by, kind: "models", version: 1, updated_at: "", rollout: { state: "applied", kine_revision: 1, applied: 1, online: 1, rejections: [] },
});
const models = [
  source("direct-a", { display_name: "default", model_name: "claude-v1", provider_key_id: "account-a" }),
  source("direct-b", { display_name: "default", model_name: "claude-v2", provider_key_id: "account-b" }, "public:public-alias"),
  source("public-alias", { display_name: "public alias", routing: { targets: [{ model_id: "direct-b" }] } }, "public:public-alias"),
  source("wildcard", { display_name: "claude-*", model_name: "*", provider_key_id: "account-a" }),
  source("ensemble", { ensemble: {}, model_name: "name", provider_key_id: "account-a" }),
];

test("fixed counter account choices follow each protocol's direct endpoint contract", () => {
  assert.equal(typeof counting.tokenCountProviderServes, "function");
  const cases = [
    [{ provider: "anthropic" }, true, false],
    [{ provider: "openai" }, false, true],
    [{ provider: "other", adapter: "anthropic" }, true, false],
    [{ provider: "other", adapter: "openai" }, false, true],
    [{ provider: "other", adapter: "openai", apis: null }, false, true],
    [{ provider: "other", adapter: "openai", apis: {} }, false, false],
    [{ provider: "other", adapter: "openai", apis: { messages: {} } }, true, false],
    [{ provider: "other", adapter: "openai", apis: { responses: null } }, false, false],
    [{ provider: "other", adapter: "openai", apis: { responses: {} } }, false, true],
    [{ provider: "other", adapter: "openai", apis: { messages: {}, responses: {} } }, true, true],
    [{ provider: "other" }, false, false],
    [{ provider: "other", apis: { messages: {} } }, true, false],
    [{ provider: "other", apis: { responses: {} } }, false, true],
    [{ provider: "openai", apis: {} }, false, false],
    [{ provider: "openai", apis: { messages: {} } }, true, false],
    [{ provider: "openai", apis: { responses: null } }, false, false],
    [{ provider: "anthropic", apis: {} }, true, false],
    [{ provider: "openai", apis: null }, false, true],
    ...["bedrock", "vertex", "azure-openai"].map((adapter) => [
      { provider: "anthropic", adapter, apis: { messages: {}, responses: {} } }, false, false,
    ] as const),
    ...["bedrock", "vertex", "azure-openai"].map((adapter) => [
      { provider: "openai", adapter }, false, false,
    ] as const),
    ...["bedrock", "vertex", "azure-openai"].map((adapter) => [
      { provider: "openai", adapter, apis: null }, false, false,
    ] as const),
  ] as const;
  for (const [account, messages, responses] of cases) {
    assert.equal(counting.tokenCountProviderServes(account, "messages_count_tokens"), messages, JSON.stringify(account));
    assert.equal(counting.tokenCountProviderServes(account, "responses_input_tokens"), responses, JSON.stringify(account));
  }
});

test("source choices preserve same-name account identity and include generated direct targets only", () => {
  assert.deepEqual(tokenCountSources(models).map((m) => m.id), ["direct-a", "direct-b"]);
});

test("selecting a source stores its ID and exact upstream snapshot, preserving the counter model name", () => {
  assert.deepEqual(selectTokenCountSource({ model_id: "direct-a", source_model_name: "claude-v1", model_name: "counter-v9" }, models[1]!), {
    model_id: "direct-b", source_model_name: "claude-v2", model_name: "counter-v9",
  });
});

test("saving one block preserves untouched blocks, mapping metadata and document fields", () => {
  const untouched = { provider_key_id: "other", model_mappings: [], future: true };
  const stored: TokenCounting = { messages_count_tokens: { provider_key_id: "before", model_mappings: [] }, responses_input_tokens: untouched, future_root: [1] };
  const updated = { provider_key_id: "after", model_mappings: [{ model_id: "direct-a", source_model_name: "claude-v1", model_name: "counter-v1", future_mapping: true }], future_target: 7 };
  assert.deepEqual(tokenCountBody(stored, { messages_count_tokens: updated }), { ...stored, messages_count_tokens: updated });
  assert.equal(tokenCountBody(stored, { messages_count_tokens: updated }).responses_input_tokens, untouched);
});

test("disabling removes only the chosen block and never invents an enabled target", () => {
  const stored = { messages_count_tokens: { provider_key_id: "account", model_mappings: [] }, future: 1 };
  assert.deepEqual(tokenCountBody(stored, { messages_count_tokens: null }), { future: 1 });
  assert.deepEqual(tokenCountBody({}, {}), {});
});

test("rejects duplicate source IDs, wildcard names, missing sources and changed source snapshots", () => {
  const mapping = { model_id: "direct-a", source_model_name: "claude-v1", model_name: "counter" };
  for (const [mappings, expected] of [
    [[mapping, mapping], "重复"],
    [[{ ...mapping, model_name: "*" }], "通配符"],
    [[{ ...mapping, model_id: "wildcard", source_model_name: "*" }], "通配符"],
    [[{ ...mapping, model_id: "missing" }], "不存在"],
    [[{ ...mapping, source_model_name: "old-version" }], "已变化"],
    [[{ ...mapping, model_id: "public-alias" }], "直连"],
    [[{ ...mapping, model_name: " " }], "填写"],
  ] as const) {
    const problems = tokenCountProblems({ messages_count_tokens: { provider_key_id: "helper", model_mappings: [...mappings] } }, models);
    assert.ok(problems.some((p) => p.message.includes(expected)), `${expected}: ${JSON.stringify(problems)}`);
  }
  assert.deepEqual(tokenCountProblems({ messages_count_tokens: { provider_key_id: "helper", model_mappings: [mapping] } }, models), []);
});

test("a fixed account without optional mappings is valid and its absent field stays absent", () => {
  const stored = { messages_count_tokens: { provider_key_id: "helper" } };
  assert.deepEqual(tokenCountProblems(stored, models), []);
  assert.deepEqual(tokenCountBody(stored, {}), stored);
});

const embedding = source("embedding-source", {
  display_name: "embedding-source",
  provider: "openai",
  model_name: "text-embedding-3-small",
  provider_key_id: "account-a",
  embedding: { dimensions: 1536 },
});

test("source choices exclude a valid embedding model even with its own provider key and upstream name", () => {
  assert.deepEqual(tokenCountSources([...models, embedding]).map((m) => m.id), ["direct-a", "direct-b"]);
});

test("a stored embedding source mapping is refused instead of being accepted as a count source", () => {
  const problems = tokenCountProblems({ messages_count_tokens: { provider_key_id: "helper", model_mappings: [{
    model_id: embedding.id, source_model_name: "text-embedding-3-small", model_name: "counter",
  }] } }, [...models, embedding]);
  assert.ok(problems.some((problem) => problem.path === "/messages_count_tokens/model_mappings/0/model_id" && problem.message.includes("嵌入")));
});
