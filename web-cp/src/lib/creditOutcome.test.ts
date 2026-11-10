import assert from "node:assert/strict";
import { test } from "node:test";
import { createCreditLocks, sendCredit } from "./creditOutcome.ts";

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

test("a 2xx answer that is not JSON: the credit outcome is unknown", async () => {
  // The request was stored (201) but an intermediary answered with a body the client could not parse.
  assert.equal((await sendCredit(fail({ status: 201, code: "bad_response" }))).kind, "unknown");
  assert.equal((await sendCredit(fail({ status: 413, code: "bad_response" }))).kind, "unknown");
});

test("success", async () => {
  assert.deepEqual(await sendCredit(() => Promise.resolve(7)), { kind: "done", value: 7 });
});

test("a tenant's credit lock is taken once, and an unknown outcome keeps it until released", () => {
  const locks = createCreditLocks();
  let seen = 0;
  const stop = locks.subscribe(() => seen++);
  assert.equal(locks.acquire("t1"), true);
  // A second form, or a second confirm, cannot send while the first is on its way.
  assert.equal(locks.acquire("t1"), false);
  assert.equal(locks.get("t1"), "pending");
  // Another tenant is not held up.
  assert.equal(locks.acquire("t2"), true);
  // Releasing is for an unknown outcome the operator checked, not for a credit still on its way.
  locks.release("t1");
  assert.equal(locks.get("t1"), "pending");

  locks.settle("t1", "unknown");
  assert.equal(locks.get("t1"), "unknown");
  assert.equal(locks.acquire("t1"), false);
  locks.release("t1");
  assert.equal(locks.get("t1"), null);
  assert.equal(locks.acquire("t1"), true);

  // A definite answer frees the tenant at once.
  locks.settle("t1", "done");
  assert.equal(locks.get("t1"), null);
  locks.settle("t2", "refused");
  assert.equal(locks.get("t2"), null);
  // Every change was told to the subscribers, so a form mounted mid-flight follows it.
  assert.equal(seen, 7);
  stop();
  locks.acquire("t3");
  assert.equal(seen, 7);
});

test("a credit refused after its form closed is told to the tenant's next form, until another credit is sent", () => {
  const locks = createCreditLocks();
  assert.equal(locks.acquire("t1"), true);
  locks.settle("t1", "refused", "上一笔入账被拒绝，未入账：不能包含控制字符");
  // The form unlocks, and says why nothing was credited.
  assert.equal(locks.get("t1"), null);
  assert.equal(locks.refusal("t1"), "上一笔入账被拒绝，未入账：不能包含控制字符");
  assert.equal(locks.refusal("t2"), null);
  // The next credit starts clean.
  assert.equal(locks.acquire("t1"), true);
  assert.equal(locks.refusal("t1"), null);
  // A refusal the sending form shows itself leaves nothing behind.
  locks.settle("t1", "refused");
  assert.equal(locks.refusal("t1"), null);
  // Only a refusal is kept: a credit that landed, or whose outcome is unknown, leaves none.
  locks.acquire("t2");
  locks.settle("t2", "done", "x");
  assert.equal(locks.refusal("t2"), null);
  locks.acquire("t3");
  locks.settle("t3", "unknown", "x");
  assert.equal(locks.refusal("t3"), null);
});
