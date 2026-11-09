import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  fundTenant,
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { callGateway, startPlatformUpstream, type PlatformUpstream } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: a priced call, end to end, in one group's money and then in another's.
// A tenant is assigned two provider groups, A and B, priced at different
// CNY-per-USD rates; a key names exactly one of them, and a call through that
// key is charged the official USD baseline times its own group's rate, rounded
// up once. `GET /v1/generation` names the currency, and the amount it reports
// is the amount the call was charged.
//
// A rate scheduled ahead of time is not in force until it starts: before it,
// the old rate is charged; after it, the new one is, from the first call of
// the new instant.
//
// Everything is seeded through the control plane's API; the only stand-in is
// the upstream model, which reports the same usage on every answer.

/** What the upstream reports on every chat: 1000 prompt and 200 completion. */
const USAGE = { prompt: 1000, cached: 0, completion: 200 };
/**
 * The baseline cards, in USD per million tokens. X charges 6000 µ$ for 1000
 * prompt and 4000 µ$ for 200 completion; Y charges 10 000 µ$ for its 1000
 * prompt. Both are 0.01 USD a call.
 */
const X_CARD = { default: { base: { input: "6", output: "20" } } };
const Y_CARD = { default: { base: { input: "10", output: "0" } } };
/** One call's cost in micro-USD, from the cards and used above: 0.01 USD. */
const MICRO_USD = 10_000;

const GROUP_A = { name: "grp-route-9f3", display: "A 分组 9f3", rate: "7.2" };
/** B's default is 6, and X is overridden to 5 in it. */
const GROUP_B = { name: "grp-gp-7c1", display: "B 分组 7c1", rate: "6" };
const X_IN_B = "5";
/** What A's default is moved to by the scheduled rate. */
const A_LATER = "8";

/** A rate's value in the engine's units: 10⁴ per unit of CNY per USD. */
function rateE4(rate: string): number {
  const [whole, frac = ""] = rate.split(".");
  return Number(whole) * 10_000 + Number(frac.padEnd(4, "0"));
}

/**
 * The charge of one call, in µ¥: the baseline's µ$ × the rate, rounded up
 * once. The engine rounds exactly once, upward.
 */
function microCny(rate: string): number {
  return Math.ceil((MICRO_USD * rateE4(rate)) / 10_000);
}

/**
 * A listed price per token, in CNY, for `usdPerMillion` (a whole number of
 * USD per million tokens) at `rate`: exact, in its shortest form.
 */
function perToken(usdPerMillion: number, rate: string): string {
  // USD per million × the rate × 10⁴ is CNY per token × 10¹⁰.
  const scaled = BigInt(usdPerMillion) * BigInt(rateE4(rate));
  const whole = scaled / 10_000_000_000n;
  const frac = (scaled % 10_000_000_000n).toString().padStart(10, "0").replace(/0+$/, "");
  return frac === "" ? whole.toString() : `${whole}.${frac}`;
}

/** `micro` µ¥ as the six-decimal yuan string the API answers with. */
function yuan(micro: number): string {
  const whole = Math.floor(micro / 1_000_000);
  const frac = String(micro % 1_000_000).padStart(6, "0");
  return `${whole}.${frac}`;
}

describe("a call is charged its own group's rate, and the answer names the currency", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: PlatformUpstream | undefined;
  let app: ManagedGateway | undefined;
  let tenantId: string;
  /** The key in A, and the key in B. */
  let keyA: string;
  let keyB: string;
  const models: Record<string, string> = {};
  const groups: Record<string, Json> = {};
  /** Distinguishes the request ids of this case's calls. */
  let sent = 0;

  /** One chat under a fresh request id; returns the id the gateway adopted. */
  async function call(key: string, model: string): Promise<string> {
    const id = `req-gp-${++sent}-9f3`;
    const answer = await callGateway(
      app!.proxyUrl,
      key,
      "/v1/chat/completions",
      { model, messages: [{ role: "user", content: "what does this cost" }] },
      { "x-aisix-request-id": id },
    );
    expect(answer.status, answer.text).toBe(200);
    expect(answer.headers.get("x-aisix-request-id"), "the gateway adopted the request id").toBe(id);
    return id;
  }

  /** Waits until `key`'s lookup of `id` reports `total`, and returns the answer's data. */
  function charged(key: string, id: string, total: string): Promise<Json> {
    let last = "no lookup";
    return pollUntil(
      `${id} to be charged ${total}`,
      async () => {
        const answer = await callGateway(
          app!.proxyUrl,
          key,
          `/v1/generation?id=${encodeURIComponent(id)}`,
          undefined,
        );
        last = `${answer.status} ${answer.text}`;
        const data = answer.json?.data as Json | undefined;
        if (answer.status !== 200 || data?.total_cost !== total) return undefined;
        const items = data.items as Json[];
        return items.length === 1 ? data : undefined;
      },
      { timeoutMs: 30_000, intervalMs: 500, describe: () => last },
    );
  }

  beforeAll(async () => {
    upstream = await startPlatformUpstream("gp-up-e2e", { usage: USAGE });
    cp = await startControlPlane();
    const gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw);
    await cp.admin.waitRegistered(gw.dpId);

    // Two groups, each with its own default rate, and one account in both so a
    // key of either group reaches the same model: the charge then differs only
    // by the group the key names.
    for (const group of [GROUP_A, GROUP_B]) {
      groups[group.name] = await cp.admin.createGroup(group.name, group.display, group.rate);
    }
    const account = await cp.admin.create("provider_keys", {
      display_name: "gp-up-e2e",
      provider: "openai",
      api_key: "sk-gp-upstream-e2e",
      api_base: `${upstream.baseUrl}/v1`,
      groups: [GROUP_A.name, GROUP_B.name],
    });

    // Two models on that account, each priced at its official baseline.
    for (const [name, card] of [
      ["gp-x-9f3", X_CARD],
      ["gp-y-7c1", Y_CARD],
    ] as const) {
      const model = await cp.admin.create("models", {
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: account.id as string,
      });
      models[name] = model.id as string;
      const priced = await cp.admin.post("/admin/pricing/baselines", {
        model_id: models[name],
        // A price cannot start more than five seconds in the past, and one that
        // started later would leave the case's first call unpriced while it
        // waited.
        effective_from: new Date(Date.now() - 4_000).toISOString(),
        card,
        note: "the group-pricing case's card",
      });
      expect(priced.status, priced.text).toBe(201);
    }
    // X is cheaper in B: an override on (B, X) beats B's default rate.
    const override = await cp.admin.post(`/admin/provider-groups/${groups[GROUP_B.name].id}/rates`, {
      model_id: models["gp-x-9f3"],
      cny_per_usd: X_IN_B,
      effective_from: new Date(Date.now() - 4_000).toISOString(),
    });
    expect(override.status, override.text).toBe(201);

    // One tenant assigned both groups, with a key in each: the keys differ only
    // in the group each names at creation.
    const created = await cp.admin.post("/admin/tenants", { name: "Group Pricing E2E Co" });
    expect(created.status, created.text).toBe(201);
    tenantId = created.body.id;
    const granted = await cp.admin.patch(
      `/admin/tenants/${tenantId}`,
      {
        allowed_model_ids: [models["gp-x-9f3"], models["gp-y-7c1"]],
        allowed_provider_groups: [GROUP_A.name, GROUP_B.name],
      },
      created.body.version,
    );
    expect(granted.status, granted.text).toBe(200);
    await fundTenant(cp, tenantId, "100.00");
    const mint = async (label: string, group: string): Promise<string> => {
      const minted = await cp!.admin.post(`/admin/tenants/${tenantId}/keys`, {
        label,
        group_id: groups[group].id,
      });
      expect(minted.status, minted.text).toBe(201);
      expect(minted.body.key.effective_provider_groups, minted.text).toEqual([group]);
      return minted.body.plaintext as string;
    };
    keyA = await mint("gp-in-a-e2e", GROUP_A.name);
    keyB = await mint("gp-in-b-e2e", GROUP_B.name);

    // The keys are minted last: each authenticating is the readiness gate.
    for (const key of [keyA, keyB]) {
      const proxy = new ProxyClient(app.proxyUrl, key);
      await pollUntil("the caller key to authenticate", async () =>
        (await proxy.listModels()).status === 200 ? true : undefined,
      );
    }
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await upstream?.close();
  });

  test("Each charge is the baseline times its group's rate, rounded up once", async () => {
    // A charges its own default rate, and B its own — or, for X, its override.
    expect(microCny(GROUP_A.rate), "0.01 USD at 7.2").toBe(72_000);
    expect(microCny(GROUP_B.rate), "0.01 USD at 6").toBe(60_000);
    expect(microCny(X_IN_B), "0.01 USD at 5").toBe(50_000);
    for (const [key, model, rate] of [
      [keyA, "gp-x-9f3", GROUP_A.rate],
      [keyA, "gp-y-7c1", GROUP_A.rate],
      [keyB, "gp-x-9f3", X_IN_B],
      [keyB, "gp-y-7c1", GROUP_B.rate],
    ] as const) {
      const id = await call(key, model);
      const data = await charged(key, id, yuan(microCny(rate)));
      expect(data.id, JSON.stringify(data)).toBe(id);
      expect(data.currency, JSON.stringify(data)).toBe("CNY");
      const items = data.items as Json[];
      expect(items, JSON.stringify(data)).toHaveLength(1);
      // The request reached the model the caller named, and was charged.
      expect(items[0].model, JSON.stringify(items[0])).toBe(model);
      expect(items[0].billing_status, JSON.stringify(items[0])).toBe("charged");
      expect(items[0].cost, JSON.stringify(items[0])).toBe(yuan(microCny(rate)));
    }
  });

  test("/v1/generation shows currency CNY and the same amounts", async () => {
    const id = await call(keyA, "gp-y-7c1");
    const total = yuan(microCny(GROUP_A.rate));
    await charged(keyA, id, total);

    const answer = await callGateway(
      app!.proxyUrl,
      keyA,
      `/v1/generation?id=${encodeURIComponent(id)}`,
      undefined,
    );
    expect(answer.status, answer.text).toBe(200);
    const data = answer.json.data as Json;
    expect(data.currency, answer.text).toBe("CNY");
    expect(data.total_cost, answer.text).toBe(total);
    expect(data.id, answer.text).toBe(id);
    const items = data.items as Json[];
    expect(items, answer.text).toHaveLength(1);
    expect(items[0].cost, answer.text).toBe(total);
    // No amount in the answer is named for another currency.
    expect(answer.text).not.toContain("_usd");

    // The same figure from the key's own lookup: the tenant has ¥100 and has
    // spent the case's calls, and the answer names its currency too.
    const key = await callGateway(app!.proxyUrl, keyA, "/v1/key", undefined);
    expect(key.status, key.text).toBe(200);
    const figures = key.json.data as Json;
    expect(figures.currency, key.text).toBe("CNY");
    expect(key.text).not.toContain("_usd");
  });

  test("Two keys of one tenant list X at their own group's price", async () => {
    /** X's pricing as `key`'s `/v1/models` lists it. */
    const listedX = async (key: string): Promise<Json | undefined> => {
      const answer = await callGateway(app!.proxyUrl, key, "/v1/models", undefined);
      expect(answer.status, answer.text).toBe(200);
      return (answer.json.data as Json[]).find((m) => m.id === "gp-x-9f3")?.pricing as Json | undefined;
    };
    // X's baseline is 6 USD per million input tokens: 0.0000432 CNY a token
    // at A's 7.2, and 0.00003 at B's override of 5.
    const inA = perToken(6, GROUP_A.rate);
    const inB = perToken(6, X_IN_B);
    expect(inA).not.toBe(inB);

    const a = await listedX(keyA);
    expect(a?.currency, JSON.stringify(a)).toBe("CNY");
    expect(a?.input, JSON.stringify(a)).toBe(inA);
    // The other key of the same tenant, right after: B's own price, never the
    // figure A's listing just cached.
    const listedAt = Date.now();
    let last: Json | undefined;
    const b = await pollUntil(
      "KB to list X at B's price",
      async () => {
        last = await listedX(keyB);
        return last?.input === inB ? last : undefined;
      },
      { timeoutMs: 10_000, intervalMs: 250, describe: () => JSON.stringify(last) },
    );
    expect(Date.now() - listedAt, "within 10 s of KA's listing").toBeLessThan(10_000);
    expect(b.currency, JSON.stringify(b)).toBe("CNY");
    // And KA still lists A's.
    expect((await listedX(keyA))?.input).toBe(inA);
  });

  test("A rate scheduled 5 s ahead applies only after it starts", async () => {
    const startsAt = new Date(Date.now() + 5_000);
    const scheduled = await cp!.admin.post(`/admin/provider-groups/${groups[GROUP_A.name].id}/rates`, {
      model_id: null,
      cny_per_usd: A_LATER,
      effective_from: startsAt.toISOString(),
    });
    expect(scheduled.status, scheduled.text).toBe(201);

    // Before it starts, the rate in force is still A's own.
    const before = await charged(keyA, await call(keyA, "gp-y-7c1"), yuan(microCny(GROUP_A.rate)));
    const alreadyNew = Date.now() >= startsAt.getTime() + 10_000;
    expect(before.currency, JSON.stringify(before)).toBe("CNY");
    if (!alreadyNew) {
      expect(before.total_cost, `${startsAt.toISOString()}: ${JSON.stringify(before)}`).toBe(
        yuan(microCny(GROUP_A.rate)),
      );
    }
    // A priced model is listed with its group's per-token price in CNY.
    const listed = await callGateway(app!.proxyUrl, keyA, "/v1/models", undefined);
    expect(listed.status, listed.text).toBe(200);
    const pricing = (listed.json.data as Json[]).find((m) => m.id === "gp-y-7c1")?.pricing as
      | Json
      | undefined;
    expect(pricing, listed.text).toBeDefined();
    expect(pricing!.currency, JSON.stringify(pricing)).toBe("CNY");
    // ¥10 per million tokens at 7.2, per token.
    expect(pricing!.input, JSON.stringify(pricing)).toBe("0.000072");

    // Past its start, the new rate is charged.
    await pollUntil(
      `${startsAt.toISOString()} to pass`,
      async () => (Date.now() > startsAt.getTime() + 1_000 ? true : undefined),
      { timeoutMs: 20_000, intervalMs: 250 },
    );
    const after = await charged(keyA, await call(keyA, "gp-y-7c1"), yuan(microCny(A_LATER)));
    expect(after.currency, JSON.stringify(after)).toBe("CNY");
    expect(microCny(A_LATER)).not.toBe(microCny(GROUP_A.rate));
  });
});
