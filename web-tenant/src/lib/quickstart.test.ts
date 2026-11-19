import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import test from "node:test";
import { gatewayBase, quickstartExample } from "./quickstart.ts";

test("示例基础地址保留部署子路径，省去凭据、查询和锚点", () => {
  assert.equal(gatewayBase("https://gateway.example.test/proxy/v1/"), "https://gateway.example.test/proxy");
  assert.equal(gatewayBase("http://127.0.0.1:3000/"), "http://127.0.0.1:3000");
  assert.equal(gatewayBase("https://private:secret@gateway.example.test/"), "https://gateway.example.test");
  assert.equal(gatewayBase("https://gateway.example.test/?credential=example"), "https://gateway.example.test");
  assert.equal(gatewayBase("https://gateway.example.test/#hidden"), "https://gateway.example.test");
});

test("cURL 按原样传递带引号的别名和部署路径", () => {
  const base = "https://gateway.example.test/p'$(printf changed)";
  const name = "客户's \"模型\" $HOME";
  const { curl } = quickstartExample(base, name, "responses");
  // 用 Bash 读取实参；不执行 HTTP 请求。
  const parsed = spawnSync("bash", ["-c", `curl() { printf '%s\\0' "$@"; }\n${curl}`], {
    encoding: "utf8",
    env: { ...process.env, ROUTIO_API_KEY: "local-test-credential" },
  });
  assert.equal(parsed.status, 0, parsed.stderr);
  const args = parsed.stdout.split("\0");
  assert.ok(args.includes(`${base}/v1/responses`));
  assert.ok(args.includes("Authorization: Bearer local-test-credential"));
  const body = args.find(arg => arg.startsWith("{"));
  assert.ok(body);
  assert.deepEqual(JSON.parse(body), { model: name, input: "Say hello in one sentence." });
});
