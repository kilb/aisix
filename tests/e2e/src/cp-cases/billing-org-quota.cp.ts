import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: an operator's organization quota blocks every key of the tenant at
// the gateway, and raising it lets them call again, across both planes.
// Everything is driven through the operator API and the real gateway; only
// the upstream model is a stand-in. Each call costs 10 000 µ$, so a $0.02
// quota is spent by two calls.

/** How long a change may take to reach the gateway. */
const WITHIN_MS = 15_000;

describe("an operator's organization quota blocks every key of the tenant", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;
  let k1: string;
  let k2: string;
  let quotaId: string;

  const chat = (key: string): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, key, "/v1/chat/completions", {
      model: CHAT_MODEL,
      messages: [{ role: "user", content: "spend a little" }],
    });
  /** Sends chats with `key`, one at a time, until one answers `status`; fails after 15 s. */
  const chatsAnswer = (key: string, status: number, what: string): Promise<GatewayAnswer> => {
    let last = "no chat sent";
    return pollUntil(
      what,
      async () => {
        const answer = await chat(key);
        last = `${answer.status} ${answer.text}`;
        return answer.status === status ? answer : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250, describe: () => last },
    );
  };

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Org Quota E2E Co", {
      billingMode: "postpaid",
      setup: (id) => fundTenant(world!.cp, id, "1.00"),
    });
    k1 = tenant.key;
    const minted = await world.cp.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: "second key" });
    expect(minted.status, minted.text).toBe(201);
    k2 = minted.body.plaintext;
    // The second key authenticates once it is in the gateway's snapshot.
    const proxy = new ProxyClient(world.gateway.proxyUrl, k2);
    await pollUntil("the second key to authenticate", async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
  });

  afterAll(async () => {
    await world?.close();
  });

  test("An operator's organization quota blocks every key of the tenant", async () => {
    const created = await world!.cp.admin.post(`/admin/tenants/${tenant.id}/budgets`, {
      kind: "periodic",
      period: "month",
      limit_usd: "0.02",
    });
    expect(created.status, created.text).toBe(201);
    expect(created.body.scope).toBe("team");
    expect(created.body.set_by).toBe("operator");
    quotaId = created.body.id;

    // Two calls, one per key, spend the $0.02. Each is served, since the
    // organization still has money when it is sent.
    for (const key of [k1, k2]) {
      const spending = await chat(key);
      expect(spending.status, spending.text).toBe(200);
    }

    for (const [name, key] of [["K1", k1], ["K2", k2]] as const) {
      const refused = await chatsAnswer(key, 429, `${name} to be refused at the organization quota`);
      expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
      expect(refused.json?.error?.scope, refused.text).toBe("team");
      expect(refused.json?.error?.scope_ref, refused.text).toBe(tenant.id);
    }
  });

  test("Raising it lets them call again", async () => {
    const raised = await world!.cp.admin.patch(`/admin/tenants/${tenant.id}/budgets/${quotaId}`, {
      limit_usd: "1.00",
    });
    expect(raised.status, raised.text).toBe(200);
    expect(raised.body.limit_micro_usd).toBe(1_000_000);
    // Settled before the answer: the quota no longer says it is spent.
    expect(raised.body.exhausted, raised.text).toBe(false);

    await chatsAnswer(k1, 200, "K1 to be served after the quota was raised");
    await chatsAnswer(k2, 200, "K2 to be served after the quota was raised");
  });
});
