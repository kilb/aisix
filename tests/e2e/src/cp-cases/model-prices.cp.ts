import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, EMBED_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { pollUntil, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";
import { startSwallowRelay, type SwallowRelay } from "../harness/swallow-relay.js";

// E2E: in platform mode, a caller's model listing shows what its own tenant
// pays per token for each model, `GET /v1/models` and `GET /v1/models/{id}` on
// the gateway, which asks the control plane for the tenant's prices. The
// gateway's REST link to the control plane (`cp_base_url`) runs through a
// relay that can hold every connection; its etcd link goes straight to the
// control plane, so configuration keeps flowing while the relay holds the
// price lookups back. Only the upstream model is a stand-in.
//
// The platform prices `chat-bill` at input 6 and output 20 USD per million
// tokens and `embed-bill` at input 10 and output 0, and leaves `chat-later`
// unpriced. The group every seeded key names charges 1 CNY per USD, so a
// per-token CNY figure is numerically the card's USD one. Tenant A pays the
// platform price; tenant C has a 20 % discount (2 000 basis points). Both are
// granted all three models.

/** The longest the gateway serves a tenant's prices from its cache. */
const PRICES_TTL_MS = 10_000;
/** How long a tenant whose prices could not be fetched is listed without them. */
const PRICES_FAILURE_TTL_MS = 2_000;
/**
 * A listing during an outage must answer well inside a second: one that
 * waited on the control plane would take as long as the outage lasts.
 */
const LISTING_DURING_OUTAGE_MS = 1_000;
const LATER_MODEL = "chat-later";

describe("in platform mode the model listing shows each model's price for the caller's tenant", () => {
  let world: BillingWorld | undefined;
  let relay: SwallowRelay | undefined;
  let tenantA: BillingTenant;
  let tenantC: BillingTenant;

  const call = (key: string, path: string): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, key, path, undefined);
  const timedListing = async (key: string): Promise<{ answer: GatewayAnswer; tookMs: number }> => {
    const started = Date.now();
    const answer = await call(key, "/v1/models");
    return { answer, tookMs: Date.now() - started };
  };
  /** The listing's models by id. */
  const byId = (answer: GatewayAnswer): Map<string, Json> =>
    new Map((answer.json.data as Json[]).map((m) => [m.id as string, m]));
  const listing = async (key: string): Promise<Map<string, Json>> => {
    const answer = await call(key, "/v1/models");
    expect(answer.status, answer.text).toBe(200);
    return byId(answer);
  };

  beforeAll(async () => {
    world = await billingWorld({
      gateway: async (cp) => {
        relay = await startSwallowRelay(new URL(cp.dpUrl).host);
        return { managed: { cp_base_url: `https://127.0.0.1:${relay.port}` } };
      },
    });
    const w = world;

    // A model the platform has not priced yet, on the same upstream account.
    const later = await w.cp.admin.create("models", {
      display_name: LATER_MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: w.models.chat.doc.provider_key_id,
    });
    const grantAll = async (id: string): Promise<void> => {
      const current = await w.cp.admin.get(`/admin/tenants/${id}`);
      const granted = await w.cp.admin.patch(
        `/admin/tenants/${id}`,
        { allowed_model_ids: [w.models.chat.id, w.models.embed.id, later.id] },
        current.version,
      );
      expect(granted.status, granted.text).toBe(200);
    };

    tenantA = await w.tenant("Model Prices A", { setup: grantAll });
    tenantC = await w.tenant("Model Prices C", {
      setup: async (id) => {
        await grantAll(id);
        const discount = await w.cp.admin.post("/admin/pricing/discounts", {
          tenant_id: id,
          effective_from: new Date().toISOString(),
          discount_bps: 2000,
        });
        expect(discount.status, discount.text).toBe(201);
      },
    });
  });

  afterAll(async () => {
    await world?.close();
    await relay?.close();
  });

  test("Each listed model carries the tenant's own per-token price", async () => {
    const a = await listing(tenantA.key);
    expect(a.get(CHAT_MODEL)?.pricing).toEqual({ currency: "CNY", input: "0.000006", output: "0.00002" });
    expect(a.get(EMBED_MODEL)?.pricing).toEqual({ currency: "CNY", input: "0.00001", output: "0" });

    // C pays 80 % of every platform price.
    const c = await listing(tenantC.key);
    expect(c.get(CHAT_MODEL)?.pricing).toEqual({ currency: "CNY", input: "0.0000048", output: "0.000016" });
    expect(c.get(EMBED_MODEL)?.pricing).toEqual({ currency: "CNY", input: "0.000008", output: "0" });
  });

  test("A model with no price is listed without pricing", async () => {
    for (const key of [tenantA.key, tenantC.key]) {
      const models = await listing(key);
      expect([...models.keys()].sort()).toEqual([CHAT_MODEL, LATER_MODEL, EMBED_MODEL].sort());
      expect(models.get(LATER_MODEL), JSON.stringify(models.get(LATER_MODEL))).not.toHaveProperty("pricing");
      // In the same listing, the priced model does carry one.
      expect(models.get(CHAT_MODEL)).toHaveProperty("pricing");
    }
  });

  test("/v1/models/{id} carries the same pricing", async () => {
    for (const key of [tenantA.key, tenantC.key]) {
      const listed = await listing(key);
      for (const model of [CHAT_MODEL, EMBED_MODEL]) {
        const one = await call(key, `/v1/models/${model}`);
        expect(one.status, one.text).toBe(200);
        expect(one.json.id, one.text).toBe(model);
        expect(one.json.pricing, one.text).toEqual(listed.get(model)?.pricing);
        expect(one.json.pricing, one.text).toBeDefined();
      }
      const unpriced = await call(key, `/v1/models/${LATER_MODEL}`);
      expect(unpriced.status, unpriced.text).toBe(200);
      expect(unpriced.json, unpriced.text).not.toHaveProperty("pricing");
    }
    // Each tenant's own price.
    const a = await call(tenantA.key, `/v1/models/${CHAT_MODEL}`);
    const c = await call(tenantC.key, `/v1/models/${CHAT_MODEL}`);
    expect(a.json.pricing.input, a.text).toBe("0.000006");
    expect(c.json.pricing.input, c.text).toBe("0.0000048");
  });

  test("With the control plane unreachable, the listing still answers within a second, without pricing", async () => {
    const before = await listing(tenantA.key);
    expect(before.get(CHAT_MODEL), "priced before the outage").toHaveProperty("pricing");
    const ids = [...before.keys()].sort();

    relay!.pause();
    try {
      // Cached prices are shown until they expire; from then on the listing
      // answers without them, and never waits on the control plane.
      let last = "no listing";
      await pollUntil(
        "A's listing to lose its prices once the cached ones expired",
        async () => {
          const { answer, tookMs } = await timedListing(tenantA.key);
          last = `${answer.status} after ${tookMs} ms: ${answer.text}`;
          expect(answer.status, last).toBe(200);
          expect(tookMs, last).toBeLessThan(LISTING_DURING_OUTAGE_MS);
          const models = [...byId(answer).values()];
          return models.every((m) => !("pricing" in m)) ? true : undefined;
        },
        { timeoutMs: PRICES_TTL_MS + 3_000, intervalMs: 250, describe: () => last },
      );

      // Every listing during the outage: the same models, no prices, fast.
      for (let i = 0; i < 3; i++) {
        const { answer, tookMs } = await timedListing(tenantA.key);
        const detail = `${answer.status} after ${tookMs} ms: ${answer.text}`;
        expect(answer.status, detail).toBe(200);
        expect(tookMs, detail).toBeLessThan(LISTING_DURING_OUTAGE_MS);
        const models = byId(answer);
        expect([...models.keys()].sort(), detail).toEqual(ids);
        for (const model of models.values()) expect(model, detail).not.toHaveProperty("pricing");
      }
    } finally {
      relay!.resume();
    }

    // Once the control plane answers again, the prices come back.
    let last = "no listing";
    await pollUntil(
      "A's listing to show its prices again",
      async () => {
        const answer = await call(tenantA.key, "/v1/models");
        last = `${answer.status}: ${answer.text}`;
        return answer.status === 200 && byId(answer).get(CHAT_MODEL)?.pricing?.input === "0.000006"
          ? true
          : undefined;
      },
      { timeoutMs: PRICES_FAILURE_TTL_MS + 5_000, intervalMs: 250, describe: () => last },
    );
  });
});
