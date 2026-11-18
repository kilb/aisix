import assert from "node:assert/strict";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createCreditLocks, matchesCreditReceipt, sendCredit } from "./creditOutcome.ts";

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

const creditBody = { operation_id: "8921671c-192f-432e-8ce1-9203f0b5ef55", amount_cny: "0.08", reason: "线下收款", reference: "BANK-1" };

test("只能用属于当前操作的已批准充值回执结束入账", () => {
  const tenant = "74727eab-82c7-4991-8beb-3f1895ece077";
  const receipt = {
    id: "f54e0770-1eb4-4a5d-b352-ce3bb681f38e", tenant_id: tenant, origin: "operator", status: "approved",
    operation_id: creditBody.operation_id, amount_micro_cny: 80_000, credit_reason: creditBody.reason, reference: creditBody.reference,
  };
  const matches = (value: unknown) => matchesCreditReceipt(value, tenant, creditBody);
  assert.equal(matches(receipt), true);
  for (const invalid of [null, {}, { ...receipt, id: "invalid" }, { ...receipt, tenant_id: "another" },
    { ...receipt, operation_id: undefined }, { ...receipt, operation_id: receipt.id }, { ...receipt, origin: "tenant" },
    { ...receipt, status: "pending" }, { ...receipt, amount_micro_cny: 80_001 },
    { ...receipt, reference: "another" }, { ...receipt, credit_reason: "" }]) {
    assert.equal(matches(invalid), false);
  }
});

test("没有有效入账回执的 HTTP 成功响应仍然保留未知结果", async (t) => {
  const bundled = await build({
    entryPoints: [fileURLToPath(new URL("./api.ts", import.meta.url))],
    bundle: true, write: false, platform: "node", format: "esm",
  });
  const { billing } = await import(`data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0]!.contents).toString("base64")}`);
  let payload = "";
  t.mock.method(globalThis, "fetch", async () => new Response(payload, { status: 201 }));
  for (payload of ["", "null", "{}", JSON.stringify({ origin: "operator", tenant_id: "elsewhere" })]) {
    const outcome = await sendCredit(() => billing.credit("tenant", creditBody));
    assert.equal(outcome.kind, "unknown");
    assert.equal((outcome.error as { code: string }).code, "bad_response");
  }
});

function tabStorage() {
  const data = new Map<string, string>();
  return {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => { data.set(key, value); },
    removeItem: (key: string) => { data.delete(key); },
  };
}

test("页面刷新保留原始入账和操作编号，重复确认与重试只能有一个在途请求", () => {
  const storage = tabStorage();
  const original = createCreditLocks(() => storage);
  assert.equal(original.acquire("t1", creditBody), true);
  assert.equal(original.retry("t1"), null);
  const restored = createCreditLocks(() => storage);
  assert.equal(restored.get("t1"), "unknown");
  assert.equal(restored.get("t2"), null);
  assert.deepEqual(restored.retry("t1"), creditBody);
  assert.equal(restored.retry("t1"), null);
  restored.settle("t1", "unknown");
  assert.deepEqual(createCreditLocks(() => storage).request("t1"), creditBody);
  assert.deepEqual(restored.retry("t1"), creditBody);
  restored.settle("t1", "done");
  assert.equal(createCreditLocks(() => storage).get("t1"), null);
});

test("入账成功后存储变为只读，解除在途状态，残留操作仍能安全重试", () => {
  const storage = tabStorage();
  const locks = createCreditLocks(() => storage);
  assert.equal(locks.acquire("t1", creditBody), true);
  storage.removeItem = () => { throw new DOMException("只读", "SecurityError"); };
  locks.settle("t1", "done");
  assert.equal(locks.get("t1"), "unknown");
  assert.deepEqual(locks.retry("t1"), creditBody);
});

test("a tenant's credit lock is taken once, and an unknown outcome keeps it until released", () => {
  const storage = tabStorage();
  const locks = createCreditLocks(() => storage);
  let seen = 0;
  const stop = locks.subscribe(() => seen++);
  assert.equal(locks.acquire("t1", creditBody), true);
  // A second form, or a second confirm, cannot send while the first is on its way.
  assert.equal(locks.acquire("t1", creditBody), false);
  assert.equal(locks.get("t1"), "pending");
  // Another tenant is not held up.
  assert.equal(locks.acquire("t2", creditBody), true);
  // Releasing is for an unknown outcome the operator checked, not for a credit still on its way.
  locks.release("t1");
  assert.equal(locks.get("t1"), "pending");

  locks.settle("t1", "unknown");
  assert.equal(locks.get("t1"), "unknown");
  assert.equal(locks.acquire("t1", creditBody), false);
  locks.release("t1");
  assert.equal(locks.get("t1"), null);
  assert.equal(locks.acquire("t1", creditBody), true);

  // A definite answer frees the tenant at once.
  locks.settle("t1", "done");
  assert.equal(locks.get("t1"), null);
  locks.settle("t2", "refused");
  assert.equal(locks.get("t2"), null);
  // Every change was told to the subscribers, so a form mounted mid-flight follows it.
  assert.equal(seen, 7);
  stop();
  locks.acquire("t3", creditBody);
  assert.equal(seen, 7);
});

test("a credit refused after its form closed is told to the tenant's next form, until another credit is sent", () => {
  const storage = tabStorage();
  const locks = createCreditLocks(() => storage);
  assert.equal(locks.acquire("t1", creditBody), true);
  locks.settle("t1", "refused", "上一笔入账被拒绝，未入账：不能包含控制字符");
  // The form unlocks, and says why nothing was credited.
  assert.equal(locks.get("t1"), null);
  assert.equal(locks.refusal("t1"), "上一笔入账被拒绝，未入账：不能包含控制字符");
  assert.equal(locks.refusal("t2"), null);
  // The next credit starts clean.
  assert.equal(locks.acquire("t1", creditBody), true);
  assert.equal(locks.refusal("t1"), null);
  // A refusal the sending form shows itself leaves nothing behind.
  locks.settle("t1", "refused");
  assert.equal(locks.refusal("t1"), null);
  // Only a refusal is kept: a credit that landed, or whose outcome is unknown, leaves none.
  locks.acquire("t2", creditBody);
  locks.settle("t2", "done", "x");
  assert.equal(locks.refusal("t2"), null);
  locks.acquire("t3", creditBody);
  locks.settle("t3", "unknown", "x");
  assert.equal(locks.refusal("t3"), null);
});
