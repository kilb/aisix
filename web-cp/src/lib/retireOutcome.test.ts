import assert from "node:assert/strict";
import { test } from "node:test";
import type { ApiError } from "./api.ts";
import {
  alreadyRetiredText,
  keptText,
  removalFailedText,
  removeThenRetire,
  retiredText,
  unknownText,
} from "./retireOutcome.ts";

// `npm run test:unit`.

/** A refusal's answer, by the fields this module reads (see its doc comment): the control plane's own 4xx. */
const refusal = (code: string, counts: Record<string, number> | null = null, status = 409) =>
  ({ message: "refused", code, counts, status }) as unknown as ApiError;

/** An answer that was lost: the client's `network`, a proxy's 502 page, or the control plane's own 500. */
const lost = {
  network: () => ({ message: "无法连接控制面", code: "network", counts: null, status: 0 }) as unknown as ApiError,
  proxy: () => ({ message: "服务端返回了非 JSON 响应（HTTP 502）", code: "bad_response", counts: null, status: 502 }) as unknown as ApiError,
  internal: () => ({ message: "internal", code: "internal", counts: null, status: 500 }) as unknown as ApiError,
};

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

test("another retire refusal: the mapping went, the model did not, and the reason is shown", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(refusal("not_found", null, 404)),
  );
  assert.equal(out.kind, "kept_failed");
  if (out.kind !== "kept_failed") throw new Error("unreachable");
  assert.match(keptText("pub-a", out), /本账号的映射已移除，但「pub-a」没有停用，仍然存在：资源不存在/);
});

test("public_model_retired at either step is the retired state, never 'not retired'", async () => {
  for (const out of [
    // Another operator retired it between the two writes.
    await removeThenRetire(
      () => Promise.resolve(),
      () => Promise.reject(refusal("public_model_retired")),
    ),
    // Or before this delete began: the mapping removal meets a retired model.
    await removeThenRetire(
      () => Promise.reject(refusal("public_model_retired")),
      () => Promise.reject(new Error("never sent")),
    ),
  ]) {
    assert.deepEqual(out, { kind: "already_retired" });
  }
  const text = alreadyRetiredText("pub-a");
  assert.match(text, /「pub-a」已经停用/);
  assert.doesNotMatch(text, /没有停用|仍然存在|没有改动/);
});

test("a lost removal answer is unknown, and the retire is never sent", async () => {
  for (const make of [lost.network, lost.proxy, lost.internal, () => new Error("boom")]) {
    let retired = false;
    const out = await removeThenRetire(
      () => Promise.reject(make()),
      () => {
        retired = true;
        return Promise.resolve();
      },
    );
    assert.equal(out.kind, "removal_unknown");
    assert.equal(retired, false);
    if (out.kind !== "removal_unknown") throw new Error("unreachable");
    // Not the refusal's "nothing changed": the answer did not say.
    const unread = unknownText("pub-a", out, null);
    assert.match(unread, /没能确认「pub-a」的删除结果/);
    assert.match(unread, /结果仍未知/);
    assert.doesNotMatch(unread, /没有改动|没有停用|已移除/);
  }
});

test("a lost retire answer is unknown, and the read-back is what is reported", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(lost.proxy()),
  );
  assert.equal(out.kind, "retire_unknown");
  if (out.kind !== "retire_unknown") throw new Error("unreachable");
  assert.match(unknownText("pub-a", out, null), /没能确认「pub-a」是否已停用.*结果仍未知/);
  assert.match(unknownText("pub-a", out, { served: false, retired: true }), /重新读取后：本账号已不再映射它，公共模型已停用/);
  assert.match(unknownText("pub-a", out, { served: false, retired: false }), /重新读取后：本账号已不再映射它，公共模型仍在用/);
  assert.doesNotMatch(unknownText("pub-a", out, { served: false, retired: true }), /没有停用|仍然存在/);
});

test("a read-back after a lost removal says what it found", async () => {
  const out = await removeThenRetire(
    () => Promise.reject(lost.network()),
    () => Promise.resolve(),
  );
  if (out.kind !== "removal_unknown") throw new Error("unreachable");
  assert.match(unknownText("pub-a", out, { served: true, retired: false }), /本账号仍映射它，公共模型仍在用/);
  assert.match(unknownText("pub-a", out, { served: false, retired: false }), /本账号已不再映射它/);
});

test("a reason that is not an API answer is printed as it is", async () => {
  const out = await removeThenRetire(
    () => Promise.resolve(),
    () => Promise.reject(new Error("boom")),
  );
  if (out.kind !== "retire_unknown") throw new Error("unreachable");
  assert.match(unknownText("pub-a", out, null), /boom/);
});

test("the success sentence names the name and both removals", () => {
  assert.equal(retiredText("pub-a"), "已删除「pub-a」：本账号的映射和它生成的模型都已移除。");
});
