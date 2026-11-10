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

// E2E: an upstream MCP server that paginates `tools/list` (one tool per page,
// chained by `nextCursor`, as the protocol's pagination allows) has every one
// of its tools listed through the gateway, on the aggregated `/mcp` endpoint
// and on the scoped `/mcp/{server}` one, and a tool granted to the caller
// that only the LAST page carries is listed and callable.

const KEY_ALL = "sk-mcp-paged-all";
const KEY_LAST_PAGE = "sk-mcp-paged-last";

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

interface RpcReply {
  status: number;
  json?: {
    result?: {
      tools?: Array<{ name: string }>;
      nextCursor?: string;
      content?: Array<{ type: string; text?: string }>;
      isError?: boolean;
    };
    error?: { code: number; message: string };
  };
}

describe("mcp tools/list e2e: a paginating upstream", () => {
  let app: SpawnedApp | undefined;
  let upstream: McpUpstream | undefined;
  let etcdReachable = false;

  const post = async (path: string, token: string, body: unknown): Promise<RpcReply> => {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
      },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    let json: RpcReply["json"];
    try {
      json = text ? JSON.parse(text) : undefined;
    } catch {
      json = undefined;
    }
    return { status: res.status, json };
  };

  const initialize = async (path: string, token: string): Promise<number> => {
    const init = await post(path, token, {
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-11-25",
        capabilities: {},
        clientInfo: { name: "mcp-pagination-e2e", version: "0.1" },
      },
    });
    if (init.status === 200) {
      await post(path, token, { jsonrpc: "2.0", method: "notifications/initialized" });
    }
    return init.status;
  };

  const listTools = async (path: string, token: string) => {
    expect(await initialize(path, token)).toBe(200);
    const r = await post(path, token, { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
    expect(r.status).toBe(200);
    expect(r.json?.error).toBeUndefined();
    return {
      names: (r.json?.result?.tools ?? []).map((t) => t.name).sort(),
      nextCursor: r.json?.result?.nextCursor,
    };
  };

  const callTool = async (path: string, token: string, name: string, text: string) => {
    expect(await initialize(path, token)).toBe(200);
    const r = await post(path, token, {
      jsonrpc: "2.0",
      id: 3,
      method: "tools/call",
      params: { name, arguments: { text } },
    });
    return r.json;
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startMcpUpstream("paged", { toolsPageSize: 1 });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.update("mcp_servers", randomUUID(), {
      display_name: "paged",
      url: upstream.url,
      enabled: true,
    });
    // Keys last, so a key authenticating implies the server has landed too.
    await seed.createApiKey({
      key_hash: sha256(KEY_ALL),
      allowed_models: [],
      mcp_access: { allow: ["*"] },
    });
    await seed.createApiKey({
      key_hash: sha256(KEY_LAST_PAGE),
      allowed_models: [],
      mcp_access: { allow: ["paged__reverse"] },
    });
    await waitConfigPropagation(async () => {
      for (const key of [KEY_ALL, KEY_LAST_PAGE]) {
        const res = await fetch(`${app!.proxyUrl}/v1/models`, {
          headers: { authorization: `Bearer ${key}` },
        });
        if (res.status !== 200) return false;
      }
      return true;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("the aggregated endpoint lists every page's tools as one complete list", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const listed = await listTools("/mcp", KEY_ALL);
    expect(listed.names).toEqual(["paged__echo", "paged__reverse"]);
    // One complete answer: nothing is left for the caller to page through.
    expect(listed.nextCursor).toBeUndefined();
  });

  test("the scoped endpoint lists every page's tools under their own names", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const listed = await listTools("/mcp/paged", KEY_ALL);
    expect(listed.names).toEqual(["echo", "reverse"]);
    expect(listed.nextCursor).toBeUndefined();
  });

  test("a granted tool only the last page carries is listed and callable", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expect((await listTools("/mcp", KEY_LAST_PAGE)).names).toEqual(["paged__reverse"]);
    expect((await listTools("/mcp/paged", KEY_LAST_PAGE)).names).toEqual(["reverse"]);

    const called = await callTool("/mcp", KEY_LAST_PAGE, "paged__reverse", "hello");
    expect(called?.error).toBeUndefined();
    expect(called?.result?.isError).not.toBe(true);
    expect(called?.result?.content?.[0]?.text).toBe("olleh");
  });
});
