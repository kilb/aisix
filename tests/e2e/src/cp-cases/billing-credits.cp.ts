import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_USD, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";

// E2E: an admin operator credits a tenant directly, to record a payment made
// outside the platform, and the credit reaches the gateway and the tenant's
// own records across both planes. Everything is driven through the operator
// and tenant APIs and the real gateway; only the upstream model is a
// stand-in.

/** How long a change may take to reach the gateway. */
const WITHIN_MS = 15_000;
const OWNER_PASSWORD = "credits-e2e-owner-7c1";
const REASON = "线下收款 7c1";

describe("operators credit tenants directly", () => {
  let world: BillingWorld | undefined;

  const chat = (tenant: BillingTenant): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, tenant.key, "/v1/chat/completions", {
      model: CHAT_MODEL,
      messages: [{ role: "user", content: "spend a little" }],
    });
  /** Sends chats, one at a time, until one answers `status`; fails after 15 s. */
  const chatsAnswer = (tenant: BillingTenant, status: number, what: string): Promise<GatewayAnswer> => {
    let last = "no chat sent";
    return pollUntil(
      what,
      async () => {
        const answer = await chat(tenant);
        last = `${answer.status} ${answer.text}`;
        return answer.status === status ? answer : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250, describe: () => last },
    );
  };
  /** The tenant's owner, invited by the operator and signed in to the portal. */
  const ownerOf = async (tenant: BillingTenant, tag: string): Promise<CpPortal> => {
    const invite = await world!.cp.admin.post(`/admin/tenants/${tenant.id}/invites`, {
      email: `owner-${tag}@credits-e2e.test`,
      role: "owner",
    });
    expect(invite.status, invite.text).toBe(201);
    const portal = world!.cp.portal();
    const accepted = await portal.post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: OWNER_PASSWORD,
      display_name: `Owner ${tag}`,
    });
    expect(accepted.status, accepted.text).toBe(201);
    return portal;
  };

  beforeAll(async () => {
    world = await billingWorld();
  });

  afterAll(async () => {
    await world?.close();
  });

  test("An operator credit settles a postpaid debt and lifts the block", async () => {
    const tenant = await world!.tenant("Credit Postpaid E2E Co", {
      billingMode: "postpaid",
      setup: async (id) => {
        const limit = await world!.cp.admin.patch(`/admin/tenants/${id}/billing`, { credit_limit_usd: "0.01" });
        expect(limit.status, limit.text).toBe(200);
      },
    });
    const owner = await ownerOf(tenant, "postpaid");

    // One call takes the balance to the credit limit; then it is refused.
    const first = await chat(tenant);
    expect(first.status, first.text).toBe(200);
    await chatsAnswer(tenant, 429, "chats to be refused at the credit limit");

    const blocked = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
    expect(blocked.status, blocked.text).toBe(200);
    expect(blocked.body.exhausted, blocked.text).toBe(true);
    const debt: number = blocked.body.balance_micro_usd;
    expect(debt, blocked.text).toBeLessThanOrEqual(-CALL_MICRO_USD);

    // The stack's operator is an admin.
    const credited = await world!.cp.admin.post(`/admin/tenants/${tenant.id}/credits`, {
      amount_usd: "0.05",
      reason: REASON,
    });
    expect(credited.status, credited.text).toBe(201);
    expect(credited.body.origin).toBe("operator");
    expect(credited.body.status).toBe("approved");
    expect(credited.body.amount_micro_usd).toBe(5 * CALL_MICRO_USD);
    expect(credited.body.credit_reason).toBe(REASON);
    expect(credited.body.requested_by).toBeNull();
    expect(credited.body.tenant_id).toBe(tenant.id);
    // Settled before the answer. A call served while the block was on its
    // way to the gateway is billed too, and its charge may still land
    // between the two reads, so the balance rose by the credit less at most
    // a few calls: never by nothing, never by two credits.
    const account = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
    expect(account.status, account.text).toBe(200);
    expect(account.body.exhausted, account.text).toBe(false);
    const rise = account.body.balance_micro_usd - debt;
    expect(rise, account.text).toBeGreaterThan(0);
    expect(rise, account.text).toBeLessThanOrEqual(5 * CALL_MICRO_USD);

    await chatsAnswer(tenant, 200, "chats to be served after the operator's credit");

    const listed = await owner.get("/api/billing/topups");
    expect(listed.status, listed.text).toBe(200);
    const items = listed.body.items as Json[];
    expect(items.map((t) => t.id), listed.text).toEqual([credited.body.id]);
    expect(items[0].origin).toBe("operator");
    expect(items[0].credit_reason).toBe(REASON);
    expect(items[0].requested_by).toBeNull();
    // The tenant learns nothing about the operator who credited it.
    expect(items[0]).not.toHaveProperty("decided_by");
    const balance = await owner.get("/api/billing/balance");
    expect(balance.body.pending_topups, balance.text).toBe(0);
  });
});
