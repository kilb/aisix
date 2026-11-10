import assert from "node:assert/strict";
import { test } from "node:test";
import { keyModels } from "./keyModels.ts";

const CHAT = "b2c1e0f4-1111-4a2b-9c3d-000000000001";
const OTHER = "b2c1e0f4-1111-4a2b-9c3d-000000000002";
const names = new Map([
  [CHAT, "chat-t6-e2e"],
  [OTHER, "other-t6-e2e"],
]);

/**
 * An `all`-scope tenant where both models are public: it reaches both, while
 * its stored list is empty (it never narrowed) — the list that must not decide.
 */
const allScope = { model_scope: "all" as const, allowed_model_ids: [], effective_model_ids: [CHAT, OTHER] };

/** A `selected`-scope tenant, granting one of the two models. */
const selected = { model_scope: "selected" as const, allowed_model_ids: [CHAT], effective_model_ids: [CHAT] };

test("an all-scope tenant never marks a selected public model as ungranted", () => {
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

test("an all-scope tenant marks a model it does not reach, whatever its stored list says", () => {
  // Narrowed to a hand-made model once, then widened back to `all`: the
  // stored list still names it, but the scope no longer reaches it, so the
  // key's selection of it is dropped from the key's document.
  const HAND = "b2c1e0f4-1111-4a2b-9c3d-000000000004";
  const widened = { model_scope: "all" as const, allowed_model_ids: [HAND], effective_model_ids: [CHAT] };
  const withHand = new Map([...names, [HAND, "hand-t6-e2e"]]);
  assert.equal(
    keyModels({ model_ids: [CHAT, HAND] }, widened, withHand),
    "chat-t6-e2e、hand-t6-e2e（租户未授权）",
  );
});

test("an id the catalogue does not hold is a deleted model, not a bare uuid", () => {
  const GONE = "b2c1e0f4-1111-4a2b-9c3d-000000000003";
  // A deleted model is in no scope, so it is ungranted under either.
  assert.equal(keyModels({ model_ids: [GONE] }, allScope, names), `${GONE}（已删除的模型）（租户未授权）`);
  assert.equal(
    keyModels({ model_ids: [GONE] }, selected, names),
    `${GONE}（已删除的模型）（租户未授权）`,
  );
});

test("an unread catalogue shows the id bare, because nothing says it is gone", () => {
  // `null` is "not read", which is not the same statement as "not found":
  // labelling a model deleted while the list is still loading would be a
  // different, false claim.
  assert.equal(keyModels({ model_ids: [CHAT] }, allScope, null), CHAT);
});
