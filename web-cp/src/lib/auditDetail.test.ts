import assert from "node:assert/strict";
import { test } from "node:test";
import { HIDDEN, redacted } from "./auditDetail.ts";

// `npm run test:unit`. No real control-plane path writes a secret-named
// field into an audit detail, so the mask is pinned here rather than in e2e.

test("secret-named fields are hidden at any depth, other fields are kept as stored", () => {
  const detail = { api_key: "sk-x", nested: { password: "p", keys: ["a"] } };
  assert.deepEqual(redacted(detail), { api_key: HIDDEN, nested: { password: HIDDEN, keys: ["a"] } });
  assert.equal(HIDDEN, "（不显示）");
});

test("the stored detail itself is not changed", () => {
  const detail = { api_key: "sk-x" };
  redacted(detail);
  assert.deepEqual(detail, { api_key: "sk-x" });
});
