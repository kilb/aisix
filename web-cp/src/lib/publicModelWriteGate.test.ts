import assert from "node:assert/strict";
import test from "node:test";
import { createPublicModelWriteGate, runPublicModelWrite } from "./publicModelWriteGate.ts";

// 第二入口在第一次释放前必须被同步拒绝，不能等 React 重绘才防止旧快照覆盖。
test("公共模型写 gate 在同一重绘前拒绝第二个写入口，释放后可重入", () => {
  const changes: boolean[] = [];
  const gate = createPublicModelWriteGate({ current: false }, (busy) => changes.push(busy));
  assert.equal(gate.begin(), true);
  assert.equal(gate.begin(), false);
  assert.deepEqual(changes, [true]);
  gate.end();
  assert.equal(gate.begin(), true);
  gate.end();
  assert.deepEqual(changes, [true, false, true, false]);
});

// 实际包装器必须阻止第二个写回调执行，并在失败后允许重新生成或再次保存。
test("保存持有期间移除和重新生成不执行，失败也释放 gate", async () => {
  const gate = createPublicModelWriteGate({ current: false }, () => undefined);
  let fail!: (reason: Error) => void;
  const pending = new Promise<void>((_resolve, reject) => { fail = reject; });
  const writes: string[] = [];
  const saving = runPublicModelWrite(gate, async () => { writes.push("save"); await pending; });
  assert.equal(await runPublicModelWrite(gate, async () => { writes.push("remove"); }), false);
  assert.equal(await runPublicModelWrite(gate, async () => { writes.push("regenerate"); }), false);
  assert.deepEqual(writes, ["save"]);
  fail(new Error("写入失败"));
  await assert.rejects(saving, /写入失败/);
  assert.equal(await runPublicModelWrite(gate, async () => { writes.push("retry"); }), true);
  assert.deepEqual(writes, ["save", "retry"]);
  assert.equal(gate.begin(), true);
  gate.end();
});
