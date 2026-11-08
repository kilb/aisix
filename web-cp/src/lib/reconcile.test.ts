import assert from "node:assert/strict";
import { test } from "node:test";
import { checkLabel, dropLabel, failureText } from "./reconcile.ts";

// Shaped like the client's `ApiError`, whose class needs a TypeScript transform to load here.
const err = (status: number) => Object.assign(new Error("raw"), { status, code: "x" });
const fallback = (e: unknown) => `fallback:${(e as Error).message}`;

test("checks and drop reasons are labelled, and unknown ones show raw", () => {
  assert.equal(checkLabel("running_balance"), "逐笔余额");
  assert.equal(checkLabel("future_check"), "future_check");
  assert.equal(dropLabel("sink_full"), "队列已满");
  assert.equal(dropLabel("send_unconfirmed"), "未确认送达（可能已入账）");
  assert.equal(dropLabel("new_reason"), "new_reason");
});

test("an unknown tenant and a timeout are worded plainly, never as a code", () => {
  assert.match(failureText(err(404), fallback), /找不到所选租户/);
  assert.match(failureText(err(503), fallback), /对账超时/);
  assert.equal(failureText(err(500), fallback), "fallback:raw");
  assert.equal(failureText(new Error("boom"), fallback), "fallback:boom");
});
