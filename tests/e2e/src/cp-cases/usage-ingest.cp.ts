import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { callGateway } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";
import { startSwallowRelay, type SwallowRelay } from "../harness/swallow-relay.js";
import { startOpenAiUpstream, type OpenAiUpstream } from "../harness/upstream-openai.js";

// E2E: usage a gateway delivers to the control plane is stored once per
// event: deduplicated by the event id the gateway mints, across a re-sent
// batch, and never by the request id a caller can choose.
//
// The gateway's REST link to the control plane (`cp_base_url`) runs through a
// relay that can lose one answer; its etcd link goes straight to the control
// plane. Heartbeats are every 300 s, so the telemetry deliveries are the only
// traffic on the relayed link.

const MODEL = "usage-chat-e2e";
const chat = { model: MODEL, messages: [{ role: "user", content: "hi" }] };

describe("usage delivered to the control plane", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: OpenAiUpstream | undefined;
  let relay: SwallowRelay | undefined;
  let app: ManagedGateway | undefined;
  let tenantId: string;
  let keyId: string;
  let key: string;

  const events = async (query: string): Promise<Json[]> => {
    const res = await cp!.admin.get(`/admin/usage/events?limit=500&${query}`);
    expect(res.status, res.text).toBe(200);
    return res.body.items;
  };
  /** Waits until `query` matches at least `n` stored events; returns them. */
  const waitEvents = (query: string, n: number) => {
    let seen = 0;
    return pollUntil(
      `${n} stored usage events for ${query}`,
      async () => {
        const items = await events(query);
        seen = items.length;
        return items.length >= n ? items : undefined;
      },
      { timeoutMs: 20_000, describe: () => `${seen} so far` },
    );
  };
  /** One chat; returns the request id the gateway answered with. */
  const send = async (headers: Record<string, string> = {}) => {
    const res = await callGateway(app!.proxyUrl, key, "/v1/chat/completions", chat, headers);
    expect(res.status, res.text).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id");
    expect(requestId).toBeTruthy();
    return requestId!;
  };

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    cp = await startControlPlane();
    relay = await startSwallowRelay(new URL(cp.dpUrl).host);
    const gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw, {
      managed: { cp_base_url: `https://127.0.0.1:${relay.port}`, heartbeat_interval_secs: 300 },
    });
    await cp.admin.waitRegistered(gw.dpId);

    const account = await cp.admin.create("provider_keys", {
      display_name: "usage-account-e2e",
      provider: "openai",
      api_key: "sk-usage-upstream-e2e",
      api_base: `${upstream.baseUrl}/v1`,
      groups: ["shared"],
    });
    const model = await cp.admin.create("models", {
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: account.id,
    });
    const tenant = await cp.admin.post("/admin/tenants", { name: "Usage E2E Co" });
    expect(tenant.status, tenant.text).toBe(201);
    tenantId = tenant.body.id;
    const granted = await cp.admin.patch(
      `/admin/tenants/${tenantId}`,
      { allowed_model_ids: [model.id] },
      tenant.body.version,
    );
    expect(granted.status, granted.text).toBe(200);
    const minted = await cp.admin.post(`/admin/tenants/${tenantId}/keys`, { label: "usage-caller" });
    expect(minted.status, minted.text).toBe(201);
    keyId = minted.body.key.id;
    key = minted.body.plaintext;

    const proxy = new ProxyClient(app.proxyUrl, key);
    await pollUntil("the caller key to authenticate", async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
  });

  afterAll(async () => {
    await app?.exit();
    await relay?.close();
    await cp?.stop();
    await upstream?.close();
  });

  test("Each request is stored exactly once, with a gateway-minted event id", async () => {
    for (let i = 0; i < 5; i++) await send();
    await waitEvents(`api_key_id=${keyId}`, 5);
    // A later request's row follows every earlier one in the gateway's queue.
    await send();
    await waitEvents(`api_key_id=${keyId}`, 6);

    const rows = await events(`api_key_id=${keyId}`);
    expect(rows.length).toBe(6);
    const ids = rows.map((r) => r.event_id);
    for (const id of ids) expect(typeof id === "string" && id.length > 0, JSON.stringify(rows)).toBe(true);
    expect(new Set(ids).size).toBe(6);
    for (const row of rows) expect(row.team_id).toBe(tenantId);
  });

  test("A batch the gateway re-sends after a lost answer is stored once", async () => {
    const first = await send();
    await waitEvents(`request_id=${first}`, 1);

    relay!.swallowNextResponse();
    const second = await send();

    let batches: Json[] = [];
    const resent = await pollUntil(
      "a batch delivered twice",
      async () => {
        const res = await cp!.admin.get("/admin/usage/batches?limit=500");
        batches = res.body.items;
        return batches.find((b) => b.deliveries >= 2);
      },
      { timeoutMs: 20_000, describe: () => JSON.stringify(batches) },
    );
    expect(relay!.stats().swallowed, "the gateway opened a fresh connection").toBe(1);

    const rows = await waitEvents(`request_id=${second}`, 1);
    expect(rows.length).toBe(1);
    expect(rows[0].batch_id).toBe(resent.batch_id);
  });

  test("A caller reusing a request id does not make two requests count once", async () => {
    const one = await send({ "x-aisix-request-id": "dup" });
    const two = await send({ "x-aisix-request-id": "dup" });
    expect([one, two]).toEqual(["dup", "dup"]);
    await waitEvents("request_id=dup", 2);
    const barrier = await send();
    await waitEvents(`request_id=${barrier}`, 1);

    const rows = await events("request_id=dup");
    expect(rows.length).toBe(2);
    expect(new Set(rows.map((r) => r.event_id)).size).toBe(2);
  });
});
