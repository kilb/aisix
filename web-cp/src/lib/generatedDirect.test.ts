import assert from "node:assert/strict";
import { test } from "node:test";
import { generatedDirect } from "./generatedDirect.ts";

const PUBLIC = "6f1c1a52-0c3e-4f57-9d1e-1b2f3a4b5c6d";
const DIRECT = "0b7e4f7a-8c2d-4e19-a6b3-9d8c7b6a5f4e";

test("a public model's direct document is one, and its routing document is not", () => {
  assert.equal(generatedDirect({ id: DIRECT, managed_by: `public:${PUBLIC}` }), true);
  assert.equal(generatedDirect({ id: PUBLIC, managed_by: `public:${PUBLIC}` }), false);
});

test("a model an operator wrote, or one another owner derived, is not one", () => {
  assert.equal(generatedDirect({ id: DIRECT, managed_by: null }), false);
  assert.equal(generatedDirect({ id: DIRECT, managed_by: `tenant:${PUBLIC}` }), false);
});
