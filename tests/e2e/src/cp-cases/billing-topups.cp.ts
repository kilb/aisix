import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_CNY, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";

// E2E: money comes into a tenant's balance as a top-up its owner requests
// in the portal and an operator approves, and only then do the tenant's
// keys call, across both planes; a rejection credits nothing. Everything is
// driven through the tenant and operator APIs and the real gateway; only
// the upstream model is a stand-in.

/** What a prepaid tenant out of money is told. */
const PREPAID_MESSAGE =
  "Insufficient balance. Ask an owner or billing member of your organization to top up.";
/** How long a change may take to reach the gateway, or a charge the ledger. */
const WITHIN_MS = 15_000;
const OWNER_PASSWORD = "topups-e2e-owner-7c1";

describe("tenants top up and operators decide", () => {
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
      email: `owner-${tag}@topups-e2e.test`,
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
  /** Requests `body` as `owner`, which must answer 201 pending; the top-up. */
  const request = async (owner: CpPortal, body: Record<string, string>): Promise<Json> => {
    const res = await owner.post("/api/billing/topups", body);
    expect(res.status, res.text).toBe(201);
    expect(res.body.status).toBe("pending");
    expect(res.body.origin).toBe("tenant");
    return res.body;
  };

  beforeAll(async () => {
    world = await billingWorld();
  });

  afterAll(async () => {
    await world?.close();
  });

  test("An approved top-up lets the tenant's keys call until the money runs out", async () => {
    const tenant = await world!.tenant("Topup Spend E2E Co");
    const owner = await ownerOf(tenant, "spend");
    // Prepaid, at 0 and with no credit limit: nothing calls yet.
    const before = await chat(tenant);
    expect(before.status, before.text).toBe(429);

    const topup = await request(owner, { amount_cny: "0.02" });
    expect(topup.amount_micro_cny).toBe(2 * CALL_MICRO_CNY);
    const queue = await world!.cp.admin.get("/admin/billing/topups?status=pending");
    expect(queue.status, queue.text).toBe(200);
    const queued = (queue.body.items as Json[]).find((t) => t.id === topup.id);
    expect(queued?.tenant_id, queue.text).toBe(tenant.id);
    expect(queued?.tenant_name, queue.text).toBe("Topup Spend E2E Co");

    const approved = await world!.cp.admin.post(`/admin/billing/topups/${topup.id}/approve`, {});
    expect(approved.status, approved.text).toBe(200);
    expect(approved.body.status).toBe("approved");

    // Two calls' worth: each is served while the balance is above 0.
    await chatsAnswer(tenant, 200, "a chat to be served after the top-up was approved");
    const second = await chat(tenant);
    expect(second.status, second.text).toBe(200);

    const spent = await pollUntil(
      "the balance to be spent down to 0",
      async () => {
        const res = await owner.get("/api/billing/balance");
        expect(res.status, res.text).toBe(200);
        return res.body.balance_micro_cny === 0 ? res.body : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250 },
    );
    expect(spent.exhausted).toBe(true);
    expect(spent.pending_topups).toBe(0);

    const refused = await chatsAnswer(tenant, 429, "chats to be refused once the top-up is spent");
    expect(refused.json?.error?.message, refused.text).toBe(PREPAID_MESSAGE);
    expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
  });

  test("A top-up of 100.50 yuan is approved and the balance becomes 100,500,000 µ¥", async () => {
    const tenant = await world!.tenant("Topup CNY E2E Co");
    const owner = await ownerOf(tenant, "cny");
    // 100.50 CNY is 100 500 000 µ¥: yuan and fen in, micro-CNY out.
    const topup = await request(owner, { amount_cny: "100.50", note: "人民币充值" });
    expect(topup.amount_micro_cny, JSON.stringify(topup)).toBe(100_500_000);
    const approved = await world!.cp.admin.post(`/admin/billing/topups/${topup.id}/approve`, {});
    expect(approved.status, approved.text).toBe(200);
    expect(approved.body.amount_micro_cny, approved.text).toBe(100_500_000);
    const balance = await owner.get("/api/billing/balance");
    expect(balance.status, balance.text).toBe(200);
    expect(balance.body.balance_micro_cny, balance.text).toBe(100_500_000);
    const ledger = await owner.get("/api/billing/ledger");
    expect(ledger.status, ledger.text).toBe(200);
    const entries = ledger.body.items as Json[];
    expect(entries.map((e) => [e.kind, e.amount_micro_cny]), ledger.text).toEqual([
      ["topup", 100_500_000],
    ]);
  });

  test("A rejected request credits nothing and the tenant sees the reason", async () => {
    const tenant = await world!.tenant("Topup Reject E2E Co");
    const owner = await ownerOf(tenant, "reject");
    const topup = await request(owner, { amount_cny: "0.05", note: "银行转账", reference: "wire-9d2" });
    const pending = await owner.get("/api/billing/balance");
    expect(pending.body.pending_topups, pending.text).toBe(1);

    const rejected = await world!.cp.admin.post(`/admin/billing/topups/${topup.id}/reject`, {
      reason: "未收到款项",
    });
    expect(rejected.status, rejected.text).toBe(200);
    expect(rejected.body.status).toBe("rejected");

    const seen = await owner.get(`/api/billing/topups/${topup.id}`);
    expect(seen.status, seen.text).toBe(200);
    expect(seen.body.status).toBe("rejected");
    expect(seen.body.reject_reason).toBe("未收到款项");
    expect(seen.body.note).toBe("银行转账");
    expect(seen.body.reference).toBe("wire-9d2");
    const listed = await owner.get("/api/billing/topups");
    expect(listed.status, listed.text).toBe(200);
    expect((listed.body.items as Json[]).map((t) => [t.id, t.reject_reason])).toEqual([[topup.id, "未收到款项"]]);

    const balance = await owner.get("/api/billing/balance");
    expect(balance.status, balance.text).toBe(200);
    expect(balance.body.balance_micro_cny, balance.text).toBe(0);
    expect(balance.body.pending_topups, balance.text).toBe(0);
    expect(balance.body.exhausted, balance.text).toBe(true);

    // Decided once: an approval after the rejection credits nothing either.
    const late = await world!.cp.admin.post(`/admin/billing/topups/${topup.id}/approve`, {});
    expect(late.status, late.text).toBe(409);
    expect(late.body.code).toBe("topup_state");
    const after = await owner.get("/api/billing/balance");
    expect(after.body.balance_micro_cny, after.text).toBe(0);
    const refused = await chat(tenant);
    expect(refused.status, refused.text).toBe(429);
  });

  test("A postpaid tenant's payment, approved as a top-up, lifts its credit-limit block", async () => {
    const tenant = await world!.tenant("Topup Postpaid E2E Co", {
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

    const topup = await request(owner, { amount_cny: "0.05", reference: "pay-4b7" });
    const approved = await world!.cp.admin.post(`/admin/billing/topups/${topup.id}/approve`, {});
    expect(approved.status, approved.text).toBe(200);
    expect(approved.body.reference).toBe("pay-4b7");
    expect(approved.body.amount_micro_cny).toBe(5 * CALL_MICRO_CNY);
    const account = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
    expect(account.body.exhausted, account.text).toBe(false);

    await chatsAnswer(tenant, 200, "chats to be served after the payment was approved");
  });
});
