import assert from "node:assert/strict";
import { test } from "node:test";
import { modelSource } from "./modelSource.ts";

// `npm run test:unit`.

const PUBLIC = "6f1c1a52-0c3e-4f57-9d1e-1b2f3a4b5c6d";
const DIRECT = "0b7e4f7a-8c2d-4e19-a6b3-9d8c7b6a5f4e";

test("an operator's model is manual", () => {
  assert.deepEqual(modelSource({ id: DIRECT, managed_by: null }), { kind: "manual" });
});

test("a public model's routing document is public, not generated, and names the model it came from", () => {
  assert.deepEqual(modelSource({ id: PUBLIC, managed_by: `public:${PUBLIC}` }), {
    kind: "public",
    id: PUBLIC,
    generated: false,
  });
});

test("a public model's direct document is public and generated, and names the same public model", () => {
  assert.deepEqual(modelSource({ id: DIRECT, managed_by: `public:${PUBLIC}` }), {
    kind: "public",
    id: PUBLIC,
    generated: true,
  });
});

test("another owner's prefix is not a public model", () => {
  assert.deepEqual(modelSource({ id: DIRECT, managed_by: `tenant:${PUBLIC}` }), { kind: "manual" });
});
