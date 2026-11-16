import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { uniquePorts } from "./cp-dev-ports.mjs";

test("automatic choices retry ports already handed out", async () => {
  const candidates = [41001, 41001, 41001, 41002];
  const choose = uniquePorts({}, async () => {
    assert.ok(candidates.length, "duplicate retry exhausted the available choices");
    return candidates.shift();
  });
  assert.equal(await choose("proxy-port"), 41001);
  assert.equal(await choose("metrics-port"), 41002);
});

test("automatic choices avoid explicit ports before their listener is started", async () => {
  const candidates = [42001, 42002];
  const choose = uniquePorts({ "proxy-port": 42001 }, async () => candidates.shift());
  assert.equal(await choose("http-port"), 42002);
  assert.equal(await choose("proxy-port"), 42001);
});

test("two explicit listeners cannot claim the same port", () => {
  assert.throws(
    () => uniquePorts({ "http-port": 43001, "dp-port": 43001 }, async () => 43002),
    /http-port.*dp-port.*43001/,
  );
});

// 真正 CLI 在创建数据库或找二进制前拒绝冲突，避免 helper 接上了却未使用。
test("the stack rejects explicit port conflicts before starting services", () => {
  const result = spawnSync(process.execPath, [
    fileURLToPath(new URL("./cp-dev-stack.mjs", import.meta.url)),
    "--http-port", "44001", "--dp-port", "44001",
  ], { env: { ...process.env, CP_E2E_PG_URL: "postgres://unused.invalid/postgres" }, encoding: "utf8" });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /http-port.*dp-port.*44001/);
  assert.doesNotMatch(result.stderr, /ENOENT|create-operator/);
});
