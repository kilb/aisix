import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_USD, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { pollUntil } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";

// E2E: an operator-set credit limit decides when a postpaid tenant is
// blocked at the gateway, and changing it (or the billing mode) moves the
// block, across both planes. Everything is driven through the operator API
// and the real gateway; only the upstream model is a stand-in.

/** What a postpaid tenant at its credit limit is told. */
const POSTPAID_MESSAGE = "Credit limit reached. Contact the platform to settle the balance.";
/** What a prepaid tenant out of money is told. */
const PREPAID_MESSAGE =
  "Account balance exhausted. Ask an owner or billing member of your organization to top up.";

const LIMIT_MICRO_USD = 30_000;
/** How long a change may take to reach the gateway. */
const WITHIN_MS = 15_000;

describe("an operator-set credit limit blocks a postpaid tenant at the gateway", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;

  const chat = (): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, tenant.key, "/v1/chat/completions", {
      model: CHAT_MODEL,
      messages: [{ role: "user", content: "spend a little" }],
    });
  const account = async () => {
    const res = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
    expect(res.status, res.text).toBe(200);
    return res.body;
  };
  /** Sends chats, one at a time, until one answers `status`; fails after 15 s. */
  const chatsAnswer = (status: number, what: string): Promise<GatewayAnswer> => {
    let last = "no chat sent";
    return pollUntil(
      what,
      async () => {
        const answer = await chat();
        last = `${answer.status} ${answer.text}`;
        return answer.status === status ? answer : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250, describe: () => last },
    );
  };

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Credit Limit E2E Co", {
      billingMode: "postpaid",
      setup: async (id) => {
        const limit = await world!.cp.admin.patch(`/admin/tenants/${id}/billing`, { credit_limit_usd: "0.03" });
        expect(limit.status, limit.text).toBe(200);
      },
    });
  });

  afterAll(async () => {
    await world?.close();
  });

  test("A postpaid tenant spends down to its credit limit and is then refused", async () => {
    const before = await account();
    expect(before.billing_mode).toBe("postpaid");
    expect(before.credit_limit_micro_usd).toBe(LIMIT_MICRO_USD);
    expect(before.exhausted).toBe(false);

    // Each call costs 10 000 µ$: three calls take the balance to the limit.
    // Sent one at a time, each is served, since the balance is above the
    // limit when it is sent.
    for (let call = 1; call <= LIMIT_MICRO_USD / CALL_MICRO_USD; call++) {
      const answer = await chat();
      expect(answer.status, `call ${call}: ${answer.text}`).toBe(200);
    }

    // The control plane's own view: the charges post, and the account says so.
    const spent = await pollUntil(
      "the balance to reach the credit limit",
      async () => {
        const a = await account();
        return a.exhausted === true ? a : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250 },
    );
    expect(spent.balance_micro_usd).toBeLessThanOrEqual(-LIMIT_MICRO_USD);
    expect(spent.available_micro_usd).toBeLessThanOrEqual(0);

    // The gateway's: chats are refused, with the credit-limit message.
    const refused = await chatsAnswer(429, "chats to be refused at the credit limit");
    expect(refused.json?.error?.message, refused.text).toBe(POSTPAID_MESSAGE);
    expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
  });

  test("Raising the credit limit lets it call again; switching it to prepaid refuses it again", async () => {
    const raised = await world!.cp.admin.patch(`/admin/tenants/${tenant.id}/billing`, { credit_limit_usd: "1.00" });
    expect(raised.status, raised.text).toBe(200);
    // Settled before the answer: the account already says it is not blocked.
    expect(raised.body.exhausted, raised.text).toBe(false);
    expect(raised.body.credit_limit_micro_usd).toBe(1_000_000);
    await chatsAnswer(200, "chats to be served after the credit limit was raised");

    const current = await world!.cp.admin.get(`/admin/tenants/${tenant.id}`);
    expect(current.status, current.text).toBe(200);
    const prepaid = await world!.cp.admin.patch(
      `/admin/tenants/${tenant.id}`,
      { billing_mode: "prepaid" },
      current.version,
    );
    expect(prepaid.status, prepaid.text).toBe(200);
    // A prepaid tenant is blocked at a balance of 0, and this one is below it.
    expect((await account()).exhausted).toBe(true);
    const refused = await chatsAnswer(429, "chats to be refused once the tenant is prepaid");
    expect(refused.json?.error?.message, refused.text).toBe(PREPAID_MESSAGE);
  });
});
