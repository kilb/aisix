import assert from "node:assert/strict";
import { test } from "node:test";
import type { ApiError } from "./api.ts";
import { keptText, removalFailedText, removeThenRetire, retiredText } from "./retireOutcome.ts";

// `npm run test:unit`.

/** A refusal's answer, by the fields this module reads (see its doc comment). */
const refusal = (code: string, counts: Record<string, number> | null = null) =>
  ({ message: "refused", code, counts }) as ApiError;

test("both writes succeed: retired", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.resolve(),
  );
  assert.deepEqual(out, { kind: "retired" });
});

test("the mapping removal fails: the retire is never attempted", async () => {
  let retired = false;
  const out = await removeThenRetire(
    () => Promise.reject(refusal("in_use")),
    () => {
      retired = true;
      return Promise.resolve();
    },
  );
  assert.equal(out.kind, "removal_failed");
  assert.equal(retired, false);
  assert.match(removalFailedText("pub-a", (out as { error: unknown }).error), /无法删除「pub-a」：该资源仍被其他资源引用/);
  assert.match(removalFailedText("pub-a", (out as { error: unknown }).error), /本账号的映射没有改动/);
});

test("the retire answers public_model_in_use: the mapping went and the model stayed, with the counts", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(refusal("public_model_in_use", { tenants: 1, keys: 2, channels: 0 })),
  );
  assert.equal(out.kind, "kept_in_use");
  if (out.kind !== "kept_in_use") throw new Error("unreachable");
  assert.deepEqual(out.counts, { tenants: 1, keys: 2, channels: 0 });
  const text = keptText("pub-a", out);
  assert.match(text, /本账号的映射已移除/);
  assert.match(text, /「pub-a」没有停用，仍然存在/);
  assert.match(text, /还有 1 个租户、2 个密钥的授权指向它/);
  assert.match(text, /从租户与密钥中移除它/);
  // Never the plain-failure voice: the success wording is not used at all.
  assert.doesNotMatch(text, /都已移除/);
});

test("the different counts in a kept_in_use answer name only what is there", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(refusal("public_model_in_use", { tenants: 0, keys: 0, channels: 2 })),
  );
  if (out.kind !== "kept_in_use") throw new Error("unreachable");
  const text = keptText("pub-a", out);
  assert.match(text, /还有 2 个启用的账号指向它/);
  assert.doesNotMatch(text, /租户/);
});

test("another retire failure: the mapping went, the model did not, and the reason is shown", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(refusal("public_model_retired")),
  );
  assert.equal(out.kind, "kept_failed");
  if (out.kind !== "kept_failed") throw new Error("unreachable");
  assert.match(keptText("pub-a", out), /本账号的映射已移除，但「pub-a」没有停用，仍然存在：该公共模型已停用/);
});

test("a reason that is not an API answer is printed as it is", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(new Error("boom")),
  );
  if (out.kind !== "kept_failed") throw new Error("unreachable");
  assert.match(keptText("pub-a", out), /boom/);
});

test("the success sentence names the name and both removals", () => {
  assert.equal(retiredText("pub-a"), "已删除「pub-a」：本账号的映射和它生成的模型都已移除。");
});
