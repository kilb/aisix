import assert from "node:assert/strict";
import { test } from "node:test";
import { sendCredit } from "./creditOutcome.ts";

// `npm run test:unit`.

const fail = (error: object) => () => Promise.reject(Object.assign(new Error("x"), error));

test("network error: the credit outcome is unknown", async () => {
  const out = await sendCredit(fail({ status: 0, code: "network" }));
  assert.equal(out.kind, "unknown");
});

test("503: the credit outcome is unknown", async () => {
  const out = await sendCredit(fail({ status: 503, code: "unavailable" }));
  assert.equal(out.kind, "unknown");
});

test("a non-ApiError exception: the credit outcome is unknown", async () => {
  assert.equal((await sendCredit(() => Promise.reject(new TypeError("boom")))).kind, "unknown");
});

test("a definite 4xx refusal: not an unknown outcome", async () => {
  assert.equal((await sendCredit(fail({ status: 422, code: "validation_failed" }))).kind, "refused");
  assert.equal((await sendCredit(fail({ status: 403, code: "forbidden" }))).kind, "refused");
});

test("success", async () => {
  assert.deepEqual(await sendCredit(() => Promise.resolve(7)), { kind: "done", value: 7 });
});
