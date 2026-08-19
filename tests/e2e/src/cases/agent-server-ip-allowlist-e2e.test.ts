import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startA2aUpstream,
  startMcpUpstream,
  waitConfigPropagation,
  type A2aUpstream,
  type McpUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// Model rows have carried a source-IP allowlist since #557, and a routing
// group's members since AISIX-Cloud#1087. MCP servers and A2A agents carried
// none — so an operator who restricted a model to a source network found the
// same restriction unavailable for the MCP server and the A2A agent sitting
// beside it in the same environment, with no error to say why.
//
// The two endpoints refuse differently, on purpose:
//
//   /a2a/<agent>   → 403. It already discloses that an agent exists by
//                    answering an ACL denial with 403, so hiding only the IP
//                    case buys nothing and makes two refusals for the same
//                    agent answer differently.
//   /mcp/<server>  → 404, and the server's tools vanish from the aggregated
//                    listing. Telling a caller a private MCP server exists but
//                    is not theirs hands them the tool inventory the allowlist
//                    exists to withhold.
//
// A unit test can only pin the negative — a oneshot request has no peer
// address at all. Only a real socket can show a MATCHING allowlist admitting
// the caller, which is the half that would break silently if the gate were
// inverted or fail-closed everywhere.

const KEY = "sk-ip-allowlist-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

describe("MCP server and A2A agent IP allowlists", () => {
  let app: SpawnedApp | undefined;
  let agent: A2aUpstream | undefined;
  let mcp: McpUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    agent = await startA2aUpstream({ cardMount: "origin" });
    mcp = await startMcpUpstream("ip-allowlist");
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    // Loopback is what a local e2e caller presents, so this allowlist MATCHES.
    await seed.update("a2a_agents", randomUUID(), {
      name: "near",
      url: agent.url,
      protocol_version: "1.0",
      auth_type: "none",
      enabled: true,
      allowed_cidrs: ["127.0.0.0/8", "::1/128"],
    });
    // A network this caller is not on, so this allowlist EXCLUDES.
    await seed.update("a2a_agents", randomUUID(), {
      name: "far",
      url: agent.url,
      protocol_version: "1.0",
      auth_type: "none",
      enabled: true,
      allowed_cidrs: ["10.99.0.0/24"],
    });
    await seed.update("mcp_servers", randomUUID(), {
      name: "near",
      url: mcp.url,
      enabled: true,
      allowed_cidrs: ["127.0.0.0/8", "::1/128"],
    });
    await seed.update("mcp_servers", randomUUID(), {
      name: "far",
      url: mcp.url,
      enabled: true,
      allowed_cidrs: ["10.99.0.0/24"],
    });
    // Last, so a gate on this key authenticating implies every row above.
    await seed.createApiKey({
      key_hash: sha256(KEY),
      allowed_models: [],
      allowed_agents: ["*"],
      allowed_tools: ["*"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await agent?.close();
    await mcp?.close();
  });

  test("an allowlist admits the caller it names and refuses the one it does not", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // Gate on the reachable agent answering: a 404 would mean the rows have
    // not propagated, and every assertion below would pass vacuously.
    await waitConfigPropagation(async () => {
      const r = await a2a("near");
      return r.status === 200;
    });

    expect(
      (await a2a("near")).status,
      "loopback is inside `127.0.0.0/8`, so the agent must serve",
    ).toBe(200);
    expect(
      (await a2a("far")).status,
      "a caller outside the allowlist must be refused",
    ).toBe(403);

    // The MCP server the caller may reach answers `initialize`; the one it may
    // not reads as absent.
    expect((await mcpInitialize("near")).status).toBe(200);
    expect(
      (await mcpInitialize("far")).status,
      "a server outside the allowlist must read as absent, not forbidden",
    ).toBe(404);

    // …and it is absent from the aggregated listing too, which is the half a
    // call-time-only gate would miss: the tools would still be published.
    const listed = await mcpAggregatedToolNames();
    expect(
      listed.some((name) => name.startsWith("near__")),
      `the reachable server's tools must be listed: ${listed.join(", ")}`,
    ).toBe(true);
    expect(
      listed.filter((name) => name.startsWith("far__")),
      "an unreachable server must not publish its tool inventory",
    ).toEqual([]);
  }, 120_000);

  async function a2a(name: string): Promise<{ status: number }> {
    const res = await fetch(`${app!.proxyUrl}/a2a/${name}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${KEY}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: "ip-1",
        method: "message/send",
        params: {
          message: {
            role: "user",
            parts: [{ kind: "text", text: "hello" }],
            messageId: "m-1",
          },
        },
      }),
    });
    await res.text();
    return { status: res.status };
  }

  async function mcpInitialize(name: string): Promise<{ status: number }> {
    const res = await fetch(`${app!.proxyUrl}/mcp/${name}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${KEY}`,
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-06-18",
          capabilities: {},
          clientInfo: { name: "e2e", version: "1" },
        },
      }),
    });
    await res.text();
    return { status: res.status };
  }

  /** Tool names the aggregated `/mcp` endpoint publishes to this caller. */
  async function mcpAggregatedToolNames(): Promise<string[]> {
    const post = async (body: unknown, sessionId?: string) => {
      const headers: Record<string, string> = {
        authorization: `Bearer ${KEY}`,
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
      };
      if (sessionId) headers["mcp-session-id"] = sessionId;
      const res = await fetch(`${app!.proxyUrl}/mcp`, {
        method: "POST",
        headers,
        body: JSON.stringify(body),
      });
      return {
        session: res.headers.get("mcp-session-id"),
        text: await res.text(),
      };
    };
    const init = await post({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-06-18",
        capabilities: {},
        clientInfo: { name: "e2e", version: "1" },
      },
    });
    const session = init.session ?? undefined;
    const list = await post(
      { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
      session,
    );
    // The body is either JSON or an SSE frame carrying it; take the JSON out
    // of whichever shape arrived.
    const payload = list.text.includes("data:")
      ? list.text
          .split("\n")
          .filter((l) => l.startsWith("data:"))
          .map((l) => l.slice(5).trim())
          .join("")
      : list.text;
    try {
      const parsed = JSON.parse(payload) as {
        result?: { tools?: Array<{ name?: string }> };
      };
      return (parsed.result?.tools ?? []).flatMap((t) =>
        typeof t.name === "string" ? [t.name] : [],
      );
    } catch {
      return [];
    }
  }
});
