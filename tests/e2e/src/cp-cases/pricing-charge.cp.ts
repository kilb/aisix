import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { callGateway, startPlatformUpstream, type GatewayAnswer, type PlatformUpstream } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";
import { startSwallowRelay, type SwallowRelay } from "../harness/swallow-relay.js";

// E2E: what a tenant's requests cost, across both planes. An operator prices
// a platform model and gives the tenant a discount; the tenant's key calls a
// real gateway; the usage the gateway delivers is priced by the control plane
// as it is stored, and every stored usage row gets exactly one charge.
//
// Everything is seeded through the control plane's API. The gateway's REST
// link to the control plane runs through a relay that can lose one answer.

/** USD per million tokens: input 1, output 2, cache read 0.1. */
const CARD = { default: { base: { input: "1", output: "2", cache_read: "0.1" } } };
/** What every chat, messages and responses answer of the upstream reports. */
const USAGE = { prompt: 1000, cached: 800, completion: 200 };
/** (200 fresh input × 1 + 800 cache reads × 0.1 + 200 output × 2) µ$, less the tenant's 20%. */
const FULL_CHARGE = 544;
/** 1000 embedded tokens × 1 µ$, less 20%. */
const EMBED_CHARGE = 800;
const FORBIDDEN = "forbidden-word";

const chat = (content: string, stream = false) => ({
  model: "priced",
  stream,
  messages: [{ role: "user", content }],
});
const messages = (content: string, stream = false) => ({
  model: "priced",
  max_tokens: 32,
  stream,
  messages: [{ role: "user", content }],
});
const responses = (input: string, stream = false) => ({ model: "priced", input, stream });

describe("every request a tenant key makes becomes one charge at the configured price", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: PlatformUpstream | undefined;
  let relay: SwallowRelay | undefined;
  let app: ManagedGateway | undefined;
  let tenantId: string;
  let keyId: string;
  let key: string;
  const models: Record<string, Json> = {};
  /** The non-streaming chat a later request repeats from the response cache. */
  const cachedChat = chat("what does a cached answer cost?");
  /** The request id of every model request the tenant's key has sent, in order. */
  const sent: string[] = [];

  /** One call with the tenant's key; returns the answer and its request id. */
  const send = async (path: string, body: unknown, headers: Record<string, string> = {}) => {
    const answer = await callGateway(app!.proxyUrl, key, path, body, headers);
    const requestId = answer.headers.get("x-aisix-request-id");
    expect(requestId, `${path} answered without a request id: ${answer.status}`).toBeTruthy();
    sent.push(requestId!);
    return { answer, requestId: requestId! };
  };
  const ok = (answer: GatewayAnswer) => expect(answer.status, answer.text).toBe(200);

  const events = async (query: string): Promise<Json[]> => {
    const res = await cp!.admin.get(`/admin/usage/events?limit=500&${query}`);
    expect(res.status, res.text).toBe(200);
    return res.body.items;
  };
  /** Waits until `query` matches at least `n` stored usage rows; returns them. */
  const waitEvents = (query: string, n: number, timeoutMs = 20_000) => {
    let seen = 0;
    return pollUntil(
      `${n} stored usage rows for ${query}`,
      async () => {
        const items = await events(query);
        seen = items.length;
        return items.length >= n ? items : undefined;
      },
      { timeoutMs, describe: () => `${seen} so far` },
    );
  };
  /** The one charge `GET /admin/pricing/charges?event_id=` lists. */
  const chargeOf = async (eventId: string): Promise<Json> => {
    const res = await cp!.admin.get(`/admin/pricing/charges?event_id=${encodeURIComponent(eventId)}`);
    expect(res.status, res.text).toBe(200);
    expect(res.body.items.length, res.text).toBe(1);
    return res.body.items[0];
  };
  /** The charge of the one usage row the request `requestId` made. */
  const chargeFor = async (requestId: string, timeoutMs?: number): Promise<Json> => {
    const rows = await waitEvents(`request_id=${encodeURIComponent(requestId)}`, 1, timeoutMs);
    expect(rows.length, JSON.stringify(rows)).toBe(1);
    return chargeOf(rows[0].event_id);
  };
  const price = (modelId: string) =>
    cp!.admin.post("/admin/pricing/sell", {
      model_id: modelId,
      effective_from: new Date().toISOString(),
      card: CARD,
    });

  beforeAll(async () => {
    upstream = await startPlatformUpstream("price-up-e2e", { usage: USAGE, embeddingTokens: 1000 });
    cp = await startControlPlane();
    relay = await startSwallowRelay(new URL(cp.dpUrl).host);
    const gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw, {
      managed: { cp_base_url: `https://127.0.0.1:${relay.port}`, heartbeat_interval_secs: 300 },
    });
    await cp.admin.waitRegistered(gw.dpId);

    const openai = await cp.admin.create("provider_keys", {
      display_name: "price-account-e2e",
      provider: "openai",
      api_key: "sk-price-upstream-e2e",
      api_base: `${upstream.baseUrl}/v1`,
      groups: ["shared"],
    });
    // Token counting needs an upstream that counts tokens.
    const anthropic = await cp.admin.create("provider_keys", {
      display_name: "price-account-ant-e2e",
      provider: "anthropic",
      adapter: "anthropic",
      api_key: "sk-ant-price-upstream-e2e",
      api_base: upstream.baseUrl,
      groups: ["shared"],
    });
    const direct = (name: string, upstreamModel: string, account: Json, provider = "openai") =>
      cp!.admin.create("models", {
        display_name: name,
        provider,
        model_name: upstreamModel,
        provider_key_id: account.id,
      });
    models.priced = await direct("priced", "gpt-4o-mini", openai);
    models.unpriced = await direct("unpriced", "gpt-4o-mini", openai);
    models.counted = await direct("counted", "claude-haiku-4-5", anthropic, "anthropic");
    await cp.admin.create("cache_policies", {
      name: "price-team-cache",
      scope: "team",
      applies_to_model_id: models.priced.id,
    });
    const guardrail = await cp.admin.create("guardrails", {
      name: "price-keyword-e2e",
      kind: "keyword",
      hook_point: "input",
      patterns: [{ kind: "literal", value: FORBIDDEN }],
    });
    await cp.admin.create("guardrail_attachments", {
      guardrail_id: guardrail.id,
      scope_type: "model",
      scope_id: models.priced.id,
      priority: 100,
    });

    const tenant = await cp.admin.post("/admin/tenants", { name: "Pricing E2E Co" });
    expect(tenant.status, tenant.text).toBe(201);
    tenantId = tenant.body.id;
    const granted = await cp.admin.patch(
      `/admin/tenants/${tenantId}`,
      { allowed_model_ids: [models.priced.id, models.unpriced.id, models.counted.id] },
      tenant.body.version,
    );
    expect(granted.status, granted.text).toBe(200);

    // Priced: `priced`, and `counted` so that a token count billed by
    // mistake would show as a charge. `unpriced` has no price.
    for (const model of [models.priced, models.counted]) {
      const priced = await price(model.id);
      expect(priced.status, priced.text).toBe(201);
    }
    const discount = await cp.admin.post("/admin/pricing/discounts", {
      tenant_id: tenantId,
      effective_from: new Date().toISOString(),
      discount_bps: 2000,
    });
    expect(discount.status, discount.text).toBe(201);

    // The key is written last; its authenticating is the readiness gate.
    const minted = await cp.admin.post(`/admin/tenants/${tenantId}/keys`, { label: "price-caller" });
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

  test("Every first-phase endpoint, streaming and not, becomes exactly one charge at the configured price", async () => {
    const calls: Array<[string, unknown]> = [
      ["/v1/chat/completions", cachedChat],
      ["/v1/chat/completions", chat("a streamed chat", true)],
      ["/v1/messages", messages("a buffered message")],
      ["/v1/messages", messages("a streamed message", true)],
      ["/v1/responses", responses("a buffered response")],
      ["/v1/responses", responses("a streamed response", true)],
    ];
    for (const [path, body] of calls) {
      const { answer, requestId } = await send(path, body);
      ok(answer);
      const charge = await chargeFor(requestId);
      const what = `${path} ${JSON.stringify(body)}: ${JSON.stringify(charge)}`;
      expect(charge.status, what).toBe("charged");
      expect(charge.charge_micro_usd, what).toBe(FULL_CHARGE);
      expect(charge.estimated, what).toBe(false);
      expect(charge.tenant_id, what).toBe(tenantId);
      expect(charge.api_key_id, what).toBe(keyId);
    }

    const embedded = await send("/v1/embeddings", { model: "priced", input: "embed this" });
    ok(embedded.answer);
    const embedCharge = await chargeFor(embedded.requestId);
    expect(embedCharge.status, JSON.stringify(embedCharge)).toBe("charged");
    expect(embedCharge.charge_micro_usd, JSON.stringify(embedCharge)).toBe(EMBED_CHARGE);

    const counted = await send("/v1/messages/count_tokens", {
      model: "counted",
      messages: [{ role: "user", content: "count these" }],
    });
    ok(counted.answer);
    const countCharge = await chargeFor(counted.requestId);
    expect(countCharge.status, JSON.stringify(countCharge)).toBe("nothing_to_bill");
    expect(countCharge.charge_micro_usd, JSON.stringify(countCharge)).toBe(0);
  });

  test("A cache hit, a stream the client abandons, and a guardrail block bill by the rules", async () => {
    const before = upstream!.count("/v1/chat/completions");
    const hit = await send("/v1/chat/completions", cachedChat);
    ok(hit.answer);
    expect(upstream!.count("/v1/chat/completions"), "the repeat reached the upstream").toBe(before);
    const hitCharge = await chargeFor(hit.requestId);
    expect(hitCharge.status, JSON.stringify(hitCharge)).toBe("exempt");
    expect(hitCharge.charge_micro_usd, JSON.stringify(hitCharge)).toBe(0);

    // The upstream streams without end and reports no usage; the client
    // leaves as soon as the answer's head arrives.
    upstream!.stallStreams = true;
    try {
      const abort = new AbortController();
      const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
        body: JSON.stringify(chat("a stream the client walks away from", true)),
        signal: abort.signal,
      });
      expect(res.status).toBe(200);
      const abandonedId = res.headers.get("x-aisix-request-id") ?? "";
      expect(abandonedId).toBeTruthy();
      sent.push(abandonedId);
      abort.abort();
      const abandoned = await chargeFor(abandonedId, 40_000);
      expect(abandoned.status, JSON.stringify(abandoned)).toBe("charged");
      expect(abandoned.estimated, JSON.stringify(abandoned)).toBe(true);
      expect(abandoned.charge_micro_usd, JSON.stringify(abandoned)).toBeGreaterThanOrEqual(1);
    } finally {
      upstream!.stallStreams = false;
    }

    const blocked = await send("/v1/chat/completions", chat(`please repeat ${FORBIDDEN}`));
    expect(blocked.answer.status, blocked.answer.text).toBeGreaterThanOrEqual(400);
    expect(blocked.answer.status, blocked.answer.text).toBeLessThan(500);
    const leaked = upstream!.requests.filter((r) => JSON.stringify(r.body).includes(FORBIDDEN));
    expect(leaked.length, "the blocked input reached the upstream").toBe(0);
    const blockCharge = await chargeFor(blocked.requestId);
    expect(blockCharge.status, JSON.stringify(blockCharge)).toBe("nothing_to_bill");
    expect(blockCharge.charge_micro_usd, JSON.stringify(blockCharge)).toBe(0);
  }, 120_000);

  test("Charges match usage rows exactly", async () => {
    // A later request's row follows every earlier one in the gateway's queue.
    const barrier = await send("/v1/chat/completions", chat("the barrier"));
    ok(barrier.answer);
    await waitEvents(`request_id=${barrier.requestId}`, 1);

    // Twelve model requests so far: six chat-family calls, embeddings and a
    // token count; the cache hit, the abandoned stream and the blocked chat;
    // and the barrier. Each is one usage row and one charge.
    expect(sent.length).toBe(12);
    const rows = await events(`api_key_id=${keyId}`);
    expect(rows.map((r) => r.request_id).sort()).toEqual([...sent].sort());
    const charges = await cp!.admin.get(`/admin/pricing/charges?limit=500&api_key_id=${keyId}`);
    expect(charges.status, charges.text).toBe(200);
    expect(charges.body.items.length, charges.text).toBe(12);
    const charged = (charges.body.items as Json[]).map((c) => c.event_id).sort();
    expect(charged).toEqual(rows.map((r) => r.event_id).sort());
  });

  test("A batch the gateway re-sends after a lost answer is charged once", async () => {
    const first = await send("/v1/chat/completions", chat("before the lost answer"));
    ok(first.answer);
    await waitEvents(`request_id=${first.requestId}`, 1);

    relay!.swallowNextResponse();
    const second = await send("/v1/chat/completions", chat("after the lost answer"));
    ok(second.answer);

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

    const rows = await waitEvents(`request_id=${second.requestId}`, 1);
    expect(rows.length).toBe(1);
    expect(rows[0].batch_id).toBe(resent.batch_id);
    const charge = await chargeOf(rows[0].event_id);
    expect(charge.status, JSON.stringify(charge)).toBe("charged");
    expect(charge.charge_micro_usd, JSON.stringify(charge)).toBe(FULL_CHARGE);
  });

  test("Usage on a model without a price is kept and shown as unpriced", async () => {
    const early = await send("/v1/chat/completions", { ...chat("no price yet"), model: "unpriced" });
    ok(early.answer);
    const earlyCharge = await chargeFor(early.requestId);
    expect(earlyCharge.status, JSON.stringify(earlyCharge)).toBe("unpriced");
    expect(earlyCharge.error_code, JSON.stringify(earlyCharge)).toBe("missing_price");
    expect(earlyCharge.charge_micro_usd, JSON.stringify(earlyCharge)).toBeNull();

    const status = await cp!.admin.get("/admin/pricing/status");
    expect(status.status, status.text).toBe(200);
    // The only request to a model without a price so far.
    expect(status.body.unpriced_last_24h, status.text).toEqual({ total: 1, by_code: { missing_price: 1 } });

    const priced = await price(models.unpriced.id);
    expect(priced.status, priced.text).toBe(201);
    const later = await send("/v1/chat/completions", { ...chat("priced now"), model: "unpriced" });
    ok(later.answer);
    const laterCharge = await chargeFor(later.requestId);
    expect(laterCharge.status, JSON.stringify(laterCharge)).toBe("charged");
    expect(laterCharge.charge_micro_usd, JSON.stringify(laterCharge)).toBe(FULL_CHARGE);

    // Charges are written once: the earlier row stays as it was priced.
    const again = await chargeOf(earlyCharge.event_id);
    expect(again.status, JSON.stringify(again)).toBe("unpriced");
    expect(again.error_code, JSON.stringify(again)).toBe("missing_price");
  });

  test("Caller-chosen request ids never merge charges", async () => {
    const one = await send("/v1/chat/completions", chat("the first of two"), { "x-aisix-request-id": "dup" });
    const two = await send("/v1/chat/completions", chat("the second of two"), { "x-aisix-request-id": "dup" });
    ok(one.answer);
    ok(two.answer);
    expect([one.requestId, two.requestId]).toEqual(["dup", "dup"]);
    await waitEvents("request_id=dup", 2);
    const barrier = await send("/v1/chat/completions", chat("after the duplicates"));
    await waitEvents(`request_id=${barrier.requestId}`, 1);

    const rows = await events("request_id=dup");
    expect(rows.length).toBe(2);
    expect(new Set(rows.map((r) => r.event_id)).size).toBe(2);
    for (const row of rows) {
      const charge = await chargeOf(row.event_id);
      expect(charge.status, JSON.stringify(charge)).toBe("charged");
      expect(charge.charge_micro_usd, JSON.stringify(charge)).toBe(FULL_CHARGE);
    }
  });
});
