import assert from "node:assert/strict";
import { test } from "node:test";
import { sendCredit } from "./creditOutcome.ts";

// `npm run test:unit`.

const fail = (error: object) => () => Promise.reject(Object.assign(new Error("x"), error));

test("网络错误：入账结果未知", async () => {
  const out = await sendCredit(fail({ status: 0, code: "network" }));
  assert.equal(out.kind, "unknown");
});

test("503：入账结果未知", async () => {
  const out = await sendCredit(fail({ status: 503, code: "unavailable" }));
  assert.equal(out.kind, "unknown");
});

test("非 ApiError 的异常：入账结果未知", async () => {
  assert.equal((await sendCredit(() => Promise.reject(new TypeError("boom")))).kind, "unknown");
});

test("明确的 4xx 拒绝：不是未知结果", async () => {
  assert.equal((await sendCredit(fail({ status: 422, code: "validation_failed" }))).kind, "refused");
  assert.equal((await sendCredit(fail({ status: 403, code: "forbidden" }))).kind, "refused");
});

test("成功", async () => {
  assert.deepEqual(await sendCredit(() => Promise.resolve(7)), { kind: "done", value: 7 });
});
