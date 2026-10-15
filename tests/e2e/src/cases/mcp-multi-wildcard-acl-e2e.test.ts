import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startMcpUpstream,
  waitConfigPropagation,
  type McpUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：MCP 工具的允许/拒绝模式里写了不止一个 `*`，照写的意思生效。
//
// `deny: ["*reverse*"]` 是拦掉「名字里带 reverse 的工具」最自然的写法。拒绝表里
// 的模式匹配不到任何东西，等于这条拒绝不存在 —— 管理员以为拦住了，其实全放行。
// 允许表一侧同理：`allowed_tools: ["*__*cho"]` 匹配不到任何东西就什么都不给。
//
// 两个真 MCP 上游（官方 TypeScript SDK），各有 echo 与 reverse 两个工具，命名空间
// 后是 alpha__echo、alpha__reverse、beta__echo、beta__reverse。

const ENV_POLICY_ID = "22222222-2222-2222-2222-22222222aaaa";

const KEY_INHERIT = "sk-mcp-multi-star-inherit";
const KEY_KEY_DENY = "sk-mcp-multi-star-key-deny";
const KEY_ALLOWLIST = "sk-mcp-multi-star-allowlist";

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

interface RpcReply {
  status: number;
  json?: {
    result?: { tools?: Array<{ name: string }>; isError?: boolean };
    error?: { code: number; message: string };
  };
}

describe("mcp tool patterns with more than one `*`", () => {
  let app: SpawnedApp | undefined;
  let alpha: McpUpstream | undefined;
  let beta: McpUpstream | undefined;
  let etcdReachable = false;

  const post = async (token: string, body: unknown): Promise<RpcReply> => {
    const res = await fetch(`${app!.proxyUrl}/mcp`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
      },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    try {
      return { status: res.status, json: text ? JSON.parse(text) : undefined };
    } catch {
      return { status: res.status };
    }
  };

  const initialize = async (token: string) => {
    await post(token, {
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-11-25",
        capabilities: {},
        clientInfo: { name: "mcp-multi-star-e2e", version: "0.1" },
      },
    });
    await post(token, { jsonrpc: "2.0", method: "notifications/initialized" });
  };

  const listToolNames = async (token: string): Promise<string[] | undefined> => {
    await initialize(token);
    const r = await post(token, { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
    return r.json?.result?.tools?.map((t) => t.name).sort();
  };

  const callSucceeds = async (token: string, name: string): Promise<boolean> => {
    await initialize(token);
    const r = await post(token, {
      jsonrpc: "2.0",
      id: 3,
      method: "tools/call",
      params: { name, arguments: { text: "hi" } },
    });
    return r.status === 200 && !r.json?.error && r.json?.result?.isError !== true;
  };

  const EXPECTED: Array<[string, string[]]> = [
    // 环境策略给了全部，再减去名字里带 reverse 的。
    [KEY_INHERIT, ["alpha__echo", "beta__echo"]],
    // 密钥自己再减去 beta 上名字里带 ch 的。
    [KEY_KEY_DENY, ["alpha__echo"]],
    // 允许表只给以 cho 结尾的工具；环境策略的拒绝照样作用于它。
    [KEY_ALLOWLIST, ["alpha__echo", "beta__echo"]],
  ];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    alpha = await startMcpUpstream("alpha");
    beta = await startMcpUpstream("beta");
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    await seed.update("mcp_servers", randomUUID(), { display_name: "alpha", url: alpha.url, enabled: true });
    await seed.update("mcp_servers", randomUUID(), { display_name: "beta", url: beta.url, enabled: true });
    await seed.update("mcp_policies", ENV_POLICY_ID, {
      scope: "env",
      mode: "all",
      deny: ["*reverse*"],
    });
    await seed.createApiKey({
      key_hash: sha256(KEY_INHERIT),
      allowed_models: [],
      mcp_access: { mode: "inherit" },
    });
    await seed.createApiKey({
      key_hash: sha256(KEY_KEY_DENY),
      allowed_models: [],
      mcp_access: { mode: "inherit", deny: ["beta__*ch*"] },
    });
    await seed.createApiKey({
      key_hash: sha256(KEY_ALLOWLIST),
      allowed_models: [],
      allowed_tools: ["*__*cho"],
    });

    // 等每把密钥的行都到位：只看到 servers 与策略、密钥还没加载时是 401。
    await waitConfigPropagation(async () => {
      for (const [token] of EXPECTED) {
        if ((await listToolNames(token)) === undefined) return false;
      }
      return true;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await alpha?.close();
    await beta?.close();
  });

  test("tools/list shows exactly what the patterns grant", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const [token, names] of EXPECTED) {
      expect(await listToolNames(token), token).toEqual(names);
    }
  });

  test("a policy deny with two `*` blocks tools/call", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expect(await callSucceeds(KEY_INHERIT, "alpha__reverse")).toBe(false);
    expect(await callSucceeds(KEY_INHERIT, "beta__reverse")).toBe(false);
    expect(await callSucceeds(KEY_INHERIT, "alpha__echo")).toBe(true);
  });

  test("a key-level deny with two `*` blocks tools/call", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expect(await callSucceeds(KEY_KEY_DENY, "beta__echo")).toBe(false);
    expect(await callSucceeds(KEY_KEY_DENY, "alpha__echo")).toBe(true);
  });

  test("an allowlist entry with two `*` grants what it names", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expect(await callSucceeds(KEY_ALLOWLIST, "beta__echo")).toBe(true);
    expect(await callSucceeds(KEY_ALLOWLIST, "beta__reverse")).toBe(false);
  });
});
