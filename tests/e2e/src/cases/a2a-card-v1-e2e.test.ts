import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startA2aUpstream,
  waitConfigPropagation,
  type A2aUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an A2A 1.0 agent card carries no top-level `url` — its endpoints are
// the `url`s inside `supportedInterfaces` (A2A v1.0.0 `AgentCard`,
// `supported_interfaces` / `AgentInterface.url`). The gateway serves such a
// card with every interface pointed at itself and without inventing a legacy
// field; a card that does carry one still has it rewritten.

const KEY = "sk-a2a-card-v1";
const MODERN = "modern-agent";
const LEGACY = "legacy-agent";

interface Card {
  name?: string;
  url?: string;
  supportedInterfaces?: Array<{ url?: string; protocolBinding?: string }>;
  skills?: Array<{ id?: string }>;
}

describe("the served card of an A2A 1.0 agent", () => {
  let app: SpawnedApp | undefined;
  let modern: A2aUpstream | undefined;
  let legacy: A2aUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    modern = await startA2aUpstream({ cardWithoutLegacyUrl: true, wireShape: "1.0" });
    legacy = await startA2aUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [name, upstream] of [
      [MODERN, modern],
      [LEGACY, legacy],
    ] as const) {
      await seed.update("a2a_agents", randomUUID(), {
        name,
        url: upstream.url,
        protocol_version: "1.0",
        auth_type: "none",
        enabled: true,
      });
    }
    await seed.createApiKey({
      key_hash: createHash("sha256").update(KEY).digest("hex"),
      allowed_models: [],
      allowed_agents: ["*"],
    });
    // Seeded last, so its key authenticating implies both agents have arrived.
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${KEY}` },
      });
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await modern?.close();
    await legacy?.close();
  });

  async function card(agent: string): Promise<{ status: number; body: Card; raw: string }> {
    const res = await fetch(`${app!.proxyUrl}/a2a/${agent}/.well-known/agent-card.json`, {
      headers: { authorization: `Bearer ${KEY}`, "x-forwarded-proto": "https" },
    });
    const raw = await res.text();
    let body: Card = {};
    try {
      body = JSON.parse(raw) as Card;
    } catch {
      // Left empty: the status assertion reports what came back.
    }
    return { status: res.status, body, raw };
  }

  test("is served, with its interfaces pointed at the gateway and no legacy url", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const served = await card(MODERN);
    expect(served.status, served.raw).toBe(200);
    const gateway = `https://${new URL(app.proxyUrl).host}/a2a/${MODERN}`;
    expect(served.body.name).toBe("Invoice Processor");
    expect("url" in served.body).toBe(false);
    expect(served.body.supportedInterfaces?.map((i) => i.url)).toEqual([gateway]);
    expect(served.body.supportedInterfaces?.[0]?.protocolBinding).toBe("JSONRPC");
    expect(served.body.skills?.[0]?.id).toBe("invoice");
    expect(served.raw).not.toContain("internal.upstream.invalid");
  });

  test("a card that carries a legacy url still has it rewritten", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const served = await card(LEGACY);
    expect(served.status, served.raw).toBe(200);
    const gateway = `https://${new URL(app.proxyUrl).host}/a2a/${LEGACY}`;
    expect(served.body.url).toBe(gateway);
    expect(served.body.supportedInterfaces?.map((i) => i.url)).toEqual([gateway]);
    expect(served.raw).not.toContain("internal.upstream.invalid");
  });
});
