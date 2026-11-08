import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_CNY, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
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

  test("A credit of 0.001 yuan answers 422 at /amount_cny", async () => {
    const tenant = await world!.tenant("Credit Fen E2E Co");
    // A third decimal is not yuan: an amount is yuan and fen at most, and the
    // refusal names the field rather than rounding.
    const refused = await world!.cp.admin.post(`/admin/tenants/${tenant.id}/credits`, {
      amount_cny: "0.001",
      reason: "精度检查 7c1",
    });
    expect(refused.status, refused.text).toBe(422);
    expect(refused.body.code, refused.text).toBe("validation_failed");
    const details = refused.body.details as { path?: string }[];
    expect(details.map((d) => d.path), refused.text).toEqual(["/amount_cny"]);
    // Nothing moved: a credit is written only once the amount is accepted.
    const balance = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
    expect(balance.body.balance_micro_cny, balance.text).toBe(0);
  });

  test("An operator credit settles a postpaid debt and lifts the block", async () => {
    const tenant = await world!.tenant("Credit Postpaid E2E Co", {
      billingMode: "postpaid",
      setup: async (id) => {
        const limit = await world!.cp.admin.patch(`/admin/tenants/${id}/billing`, { credit_limit_cny: "0.01" });
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
    // Calls served while the block was on its way to the gateway are billed
    // too. The credit below (five calls, over a limit of one) lifts the block
    // only while they number four or fewer.
    expect(
      blocked.body.balance_micro_cny,
      `more calls were served past the credit limit than this scenario's credit covers: ${blocked.text}`,
    ).toBeGreaterThan(-6 * CALL_MICRO_CNY);

    // The stack's operator is an admin.
    const credited = await world!.cp.admin.post(`/admin/tenants/${tenant.id}/credits`, {
      amount_cny: "0.05",
      reason: REASON,
    });
    expect(credited.status, credited.text).toBe(201);
    expect(credited.body.origin).toBe("operator");
    expect(credited.body.status).toBe("approved");
    expect(credited.body.amount_micro_cny).toBe(5 * CALL_MICRO_CNY);
    expect(credited.body.credit_reason).toBe(REASON);
    expect(credited.body.requested_by).toBeNull();
    expect(credited.body.tenant_id).toBe(tenant.id);
    // The credit is one topup entry of exactly the amount credited, posted on
    // top of the entry before it. Entries never change, so this holds however
    // many overshoot charges land around it.
    const ledger = await owner.get("/api/billing/ledger");
    expect(ledger.status, ledger.text).toBe(200);
    const entries = ledger.body.items as Json[]; // newest first
    const credits = entries.filter((e) => e.kind === "topup");
    expect(credits.map((e) => [e.amount_micro_cny, e.topup_id]), ledger.text).toEqual([
      [5 * CALL_MICRO_CNY, credited.body.id],
    ]);
    const before = entries[entries.indexOf(credits[0]) + 1]; // the first chat's debit at the latest
    expect(before, ledger.text).toBeDefined();
    expect(credits[0].balance_after_micro_cny - 5 * CALL_MICRO_CNY, ledger.text).toBe(
      before.balance_after_micro_cny,
    );

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
