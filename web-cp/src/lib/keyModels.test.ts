import assert from "node:assert/strict";
import { test } from "node:test";
import { keyModels } from "./keyModels.ts";

const CHAT = "b2c1e0f4-1111-4a2b-9c3d-000000000001";
const OTHER = "b2c1e0f4-1111-4a2b-9c3d-000000000002";
const names = new Map([
  [CHAT, "chat-t6-e2e"],
  [OTHER, "other-t6-e2e"],
]);

/** An `all`-scope tenant: every public model, and therefore no stored list. */
const allScope = { model_scope: "all" as const, allowed_model_ids: [] };

/** A `selected`-scope tenant, granting one of the two models. */
const selected = { model_scope: "selected" as const, allowed_model_ids: [CHAT] };

test("an all-scope tenant never marks a selected model as ungranted", () => {
  // The regression: `allowed_model_ids` is empty by definition here, so
  // testing the list alone labelled every model on every key.
  assert.equal(
    keyModels({ model_ids: [CHAT, OTHER] }, allScope, names),
    "chat-t6-e2e、other-t6-e2e",
  );
});

test("a selected-scope tenant marks exactly the models it no longer grants", () => {
  assert.equal(
    keyModels({ model_ids: [CHAT, OTHER] }, selected, names),
    "chat-t6-e2e、other-t6-e2e（租户未授权）",
  );
  // The granted one carries no mark — this is what makes the assertion
  // above about the RIGHT model rather than about the suffix existing.
  assert.equal(keyModels({ model_ids: [CHAT] }, selected, names), "chat-t6-e2e");
});

test("a key that follows every grant names no list", () => {
  assert.equal(keyModels({ model_ids: null }, allScope, names), "全部授权模型");
  assert.equal(keyModels({ model_ids: null }, selected, names), "全部授权模型");
});

test("an empty selection is none, and a name pattern is shown as written", () => {
  assert.equal(keyModels({ model_ids: [] }, allScope, names), "无");
  assert.equal(
    keyModels({ allowed_models: ["chat-t6-*"], model_ids: [CHAT] }, selected, names),
    "chat-t6-*",
  );
});

test("an id with no catalog entry falls back to the id, still unmarked on all scope", () => {
  const GONE = "b2c1e0f4-1111-4a2b-9c3d-000000000003";
  assert.equal(keyModels({ model_ids: [GONE] }, allScope, names), GONE);
  assert.equal(keyModels({ model_ids: [GONE] }, selected, names), `${GONE}（租户未授权）`);
});
