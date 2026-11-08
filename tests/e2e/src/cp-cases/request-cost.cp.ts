import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";
import { startSwallowRelay, type SwallowRelay } from "../harness/swallow-relay.js";

// E2E: a caller looks up what one request cost its organization, by the
// request id it sent, with its own API key: `GET /v1/generation?id=` on the
// gateway, which asks the control plane for it. The gateway's REST link to
// the control plane (`cp_base_url`) runs through a relay that can hold every
// connection; its etcd link goes straight to the control plane, so
// configuration keeps flowing while the relay holds the lookups back. Only
// the upstream model is a stand-in: every chat reports 1000 prompt and 200
// completion tokens and costs $0.01.

/** How long the gateway waits for the control plane on a lookup. */
const ACCOUNT_TIMEOUT_MS = 2_000;
/** How long the gateway serves a found request's answer from its cache. */
const REQUEST_TTL_MS = 3_000;
/**
 * A chat during the outage must answer well inside the lookup's deadline: a
 * model request that waited on the control plane would take the whole
 * deadline. The margin is for a loaded machine.
 */
const CHAT_DURING_OUTAGE_MS = 1_500;
const PASSWORD = "request-cost-e2e-pass-8e1";

/** The owner's request, the one every case asks about. */
const OWNERS_REQUEST = "req-e2e-7c1";
/** An id nobody ever sent. */
const NEVER_SENT = "req-never-sent-3a1";
/** The member's own request. */
const MEMBERS_REQUEST = "req-e2e-km-5b2";

describe("a caller looks up what one request cost with its API key", () => {
  let world: BillingWorld | undefined;
  let relay: SwallowRelay | undefined;
  let tenantB: BillingTenant;
  /** The owner's key. */
  let ko: string;
  /** Member M's key. */
  let km: string;

  const lookup = (key: string, id: string): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, key, `/v1/generation?id=${encodeURIComponent(id)}`, undefined);
  const timedLookup = async (key: string, id: string): Promise<{ answer: GatewayAnswer; tookMs: number }> => {
    const started = Date.now();
    const answer = await lookup(key, id);
    return { answer, tookMs: Date.now() - started };
  };
  const chat = (key: string, requestId?: string): Promise<GatewayAnswer> =>
    callGateway(
      world!.gateway.proxyUrl,
      key,
      "/v1/chat/completions",
      { model: CHAT_MODEL, messages: [{ role: "user", content: "what did this cost" }] },
      requestId ? { "x-aisix-request-id": requestId } : {},
    );
  const timedChat = async (key: string): Promise<{ answer: GatewayAnswer; tookMs: number }> => {
    const started = Date.now();
    const answer = await chat(key);
    return { answer, tookMs: Date.now() - started };
  };
  /** A chat under `requestId`, which the gateway must adopt as the request's id. */
  const served = async (key: string, requestId: string): Promise<void> => {
    const answer = await chat(key, requestId);
    expect(answer.status, answer.text).toBe(200);
    expect(answer.headers.get("x-aisix-request-id"), "the gateway adopted the caller's request id").toBe(requestId);
  };
  /** Polls `key`'s lookup of `id` every 500 ms, for at most 10 s, until it answers 200. */
  const found = async (key: string, id: string): Promise<{ answer: GatewayAnswer; data: Json }> => {
    let last = "no lookup";
    return pollUntil(
      `the lookup of ${id} to find the request`,
      async () => {
        const answer = await lookup(key, id);
        last = `${answer.status} ${answer.text}`;
        return answer.status === 200 ? { answer, data: answer.json?.data } : undefined;
      },
      { timeoutMs: 10_000, intervalMs: 500, describe: () => last },
    );
  };

  async function seat(tenantId: string, email: string, role: string): Promise<{ portal: CpPortal; id: string }> {
    const invite = await world!.cp.admin.post(`/admin/tenants/${tenantId}/invites`, { email, role });
    expect(invite.status, invite.text).toBe(201);
    const accepted = await world!.cp.portal().post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: PASSWORD,
    });
    expect(accepted.status, accepted.text).toBe(201);
    const members = await world!.cp.admin.get(`/admin/tenants/${tenantId}/members`);
    const id = (members.body.items as Json[]).find((m) => m.email === email).id as string;
    return { portal: world!.cp.portal(), id };
  }

  async function mint(tenantId: string, label: string, memberId: string): Promise<string> {
    const minted = await world!.cp.admin.post(`/admin/tenants/${tenantId}/keys`, { label, member_id: memberId });
    expect(minted.status, minted.text).toBe(201);
    const proxy = new ProxyClient(world!.gateway.proxyUrl, minted.body.plaintext);
    await pollUntil(`${label} to authenticate`, async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
    return minted.body.plaintext;
  }

  beforeAll(async () => {
    world = await billingWorld({
      gateway: async (cp) => {
        relay = await startSwallowRelay(new URL(cp.dpUrl).host);
        return { managed: { cp_base_url: `https://127.0.0.1:${relay.port}` } };
      },
    });
    const w = world;

    // Tenant A: $1.00 to spend, an owner holding KO and a member M holding KM.
    const tenantA = await w.tenant("Request Cost A", { setup: (id) => fundTenant(w.cp, id, "1.00") });
    const owner = await seat(tenantA.id, "owner@request-cost-e2e.test", "owner");
    const member = await seat(tenantA.id, "member@request-cost-e2e.test", "member");
    ko = await mint(tenantA.id, "ko", owner.id);
    km = await mint(tenantA.id, "km", member.id);

    // Tenant B: $1.00 to spend; its key belongs to no member.
    tenantB = await w.tenant("Request Cost B", { setup: (id) => fundTenant(w.cp, id, "1.00") });
  });

  afterAll(async () => {
    await world?.close();
    await relay?.close();
  });

  test("The cost of one request, by its request id", async () => {
    // The customer's keys are confined to the model endpoints, and the
    // lookup is still theirs to call.
    const images = await callGateway(world!.gateway.proxyUrl, ko, "/v1/images/generations", {
      model: CHAT_MODEL,
      prompt: "a lighthouse",
    });
    expect(images.status, images.text).toBe(403);

    await served(ko, OWNERS_REQUEST);
    const { answer, data } = await found(ko, OWNERS_REQUEST);

    // One request, one attempt: 1000 input and 200 output tokens, $0.01.
    expect(Object.keys(data).sort(), answer.text).toEqual(["has_more", "id", "items", "pending", "total_cost"]);
    expect(data.id, answer.text).toBe(OWNERS_REQUEST);
    expect(data.total_cost, answer.text).toBe("0.010000");
    expect(data.pending, answer.text).toBe(false);
    expect(data.has_more, answer.text).toBe(false);
    expect(data.items, answer.text).toHaveLength(1);
    const item = data.items[0];
    expect(Object.keys(item).sort(), answer.text).toEqual([
      "billing_status",
      "cost",
      "created_at",
      "error",
      "estimated",
      "model",
      "quantities",
      "status",
    ]);
    expect(item.model, answer.text).toBe(CHAT_MODEL);
    expect(item.status, answer.text).toBe("succeeded");
    expect(item.error, answer.text).toBeNull();
    expect(item.billing_status, answer.text).toBe("charged");
    expect(item.cost, answer.text).toBe("0.010000");
    expect(item.estimated, answer.text).toBe(false);
    expect(item.quantities, answer.text).toEqual({ input: 1000, output: 200 });
    expect(Number.isNaN(Date.parse(item.created_at)), answer.text).toBe(false);

    // Nothing of the platform's channel: not the upstream model, the account
    // or the provider behind it.
    for (const channel of ["gpt-4o-mini", "billing-account-e2e", "openai", "cost_usd", "charge_usd"]) {
      expect(answer.text).not.toContain(channel);
    }
  });

  test("A member's key does not see the owner's request", async () => {
    const never = await lookup(km, NEVER_SENT);
    expect(never.status, never.text).toBe(404);
    expect(never.json?.error?.code, never.text).toBe("not_found");

    const owners = await lookup(km, OWNERS_REQUEST);
    expect(owners.status, owners.text).toBe(404);
    expect(owners.text).toBe(never.text);
    expect(owners.headers.get("content-type")).toBe(never.headers.get("content-type"));

    // The member's key does find a request of its own.
    await served(km, MEMBERS_REQUEST);
    const { answer, data } = await found(km, MEMBERS_REQUEST);
    expect(data.id, answer.text).toBe(MEMBERS_REQUEST);
    expect(data.items, answer.text).toHaveLength(1);
    expect(data.total_cost, answer.text).toBe("0.010000");
  });

  test("Another tenant's key gets the same 404", async () => {
    const never = await lookup(tenantB.key, NEVER_SENT);
    expect(never.status, never.text).toBe(404);
    const owners = await lookup(tenantB.key, OWNERS_REQUEST);
    expect(owners.status, owners.text).toBe(404);
    expect(owners.text).toBe(never.text);
    expect(owners.headers.get("content-type")).toBe(never.headers.get("content-type"));
    // The same answer any key gets for an id nobody sent.
    const membersNever = await lookup(km, NEVER_SENT);
    expect(owners.text).toBe(membersNever.text);

    // Tenant B reusing the owner's request id finds its own request alone.
    await served(tenantB.key, OWNERS_REQUEST);
    const { answer, data } = await found(tenantB.key, OWNERS_REQUEST);
    expect(data.items, answer.text).toHaveLength(1);
    expect(data.total_cost, answer.text).toBe("0.010000");
  });

  test("With the control plane unreachable, the lookup answers 503", async () => {
    relay!.pause();
    try {
      // A found request is served from the cache until it expires; the first
      // lookup that has to ask the control plane fails within the lookup's
      // own deadline.
      let last = "no lookup";
      const failed = await pollUntil(
        "the owner's lookup to fail once its cached answer expired",
        async () => {
          const { answer, tookMs } = await timedLookup(ko, OWNERS_REQUEST);
          last = `${answer.status} after ${tookMs} ms: ${answer.text}`;
          return answer.status === 200 ? undefined : { status: answer.status, tookMs, code: answer.json?.error?.code };
        },
        { timeoutMs: REQUEST_TTL_MS + ACCOUNT_TIMEOUT_MS + 2_000, intervalMs: 250, describe: () => last },
      );
      expect(failed.status, last).toBe(503);
      expect(failed.code, last).toBe("control_plane_unavailable");
      expect(failed.tookMs, last).toBeLessThanOrEqual(ACCOUNT_TIMEOUT_MS + 1_000);

      // While a lookup waits on the held control plane, a chat on the same key
      // is served, and waits on nothing the lookup waits on. A lookup that
      // returns at once was answered without asking the control plane, so
      // the pair is sent again until its lookup is one that waited.
      let pairLast = "no pair";
      const pair = await pollUntil(
        "a lookup that waits on the held control plane, beside a chat",
        async () => {
          const [looked, chatted] = await Promise.all([timedLookup(ko, OWNERS_REQUEST), timedChat(ko)]);
          pairLast =
            `lookup ${looked.answer.status} after ${looked.tookMs} ms: ${looked.answer.text}; ` +
            `chat ${chatted.answer.status} after ${chatted.tookMs} ms`;
          expect(chatted.answer.status, chatted.answer.text).toBe(200);
          expect(chatted.tookMs, pairLast).toBeLessThan(CHAT_DURING_OUTAGE_MS);
          return looked.tookMs >= ACCOUNT_TIMEOUT_MS - 250 ? looked : undefined;
        },
        { timeoutMs: 10_000, intervalMs: 300, describe: () => pairLast },
      );
      expect(pair.answer.status, pairLast).toBe(503);
      expect(pair.answer.json?.error?.code, pairLast).toBe("control_plane_unavailable");
      expect(pair.tookMs, pairLast).toBeLessThanOrEqual(ACCOUNT_TIMEOUT_MS + 1_000);
    } finally {
      relay!.resume();
    }
  });
});
