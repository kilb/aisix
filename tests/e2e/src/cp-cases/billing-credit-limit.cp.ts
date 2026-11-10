import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_CNY, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { pollUntil } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";

// E2E: an operator-set credit limit decides when a tenant is blocked at the
// gateway, across both planes: it is an overdraft in either billing mode, so
// the block falls where the balance plus the limit reaches 0, changing the
// limit moves it, and changing the billing mode changes what the refusal
// says. Everything is driven through the operator API and the real gateway;
// only the upstream model is a stand-in.

/** What a postpaid tenant at its credit limit is told. */
const POSTPAID_MESSAGE = "Credit limit reached. Contact the platform to settle the balance.";
/** What a prepaid tenant out of money is told. */
const PREPAID_MESSAGE =
  "Insufficient balance. Ask an owner or billing member of your organization to top up.";

const LIMIT_MICRO_USD = 30_000;
/** How long a change may take to reach the gateway. */
const WITHIN_MS = 15_000;

describe("an operator-set credit limit blocks a tenant at the gateway in either billing mode", () => {
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
  /**
   * Sends chats, one at a time, until one answers `status` (and, when
   * `message` is given, an error with that message); fails after 15 s.
   */
  const chatsAnswer = (status: number, what: string, message?: string): Promise<GatewayAnswer> => {
    let last = "no chat sent";
    return pollUntil(
      what,
      async () => {
        const answer = await chat();
        last = `${answer.status} ${answer.text}`;
        const matches = answer.status === status && (message === undefined || answer.json?.error?.message === message);
        return matches ? answer : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250, describe: () => last },
    );
  };

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Credit Limit E2E Co", {
      billingMode: "postpaid",
      setup: async (id) => {
        const limit = await world!.cp.admin.patch(`/admin/tenants/${id}/billing`, { credit_limit_cny: "0.03" });
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
    expect(before.credit_limit_micro_cny).toBe(LIMIT_MICRO_USD);
    expect(before.exhausted).toBe(false);

    // Each call costs 10 000 µ$: three calls take the balance to the limit.
    // Sent one at a time, each is served, since the balance is above the
    // limit when it is sent.
    for (let call = 1; call <= LIMIT_MICRO_USD / CALL_MICRO_CNY; call++) {
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
    expect(spent.balance_micro_cny).toBeLessThanOrEqual(-LIMIT_MICRO_USD);
    expect(spent.available_micro_cny).toBeLessThanOrEqual(0);

    // The gateway's: chats are refused, with the credit-limit message.
    const refused = await chatsAnswer(429, "chats to be refused at the credit limit");
    expect(refused.json?.error?.message, refused.text).toBe(POSTPAID_MESSAGE);
    expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
  });

  test("Switching it to prepaid keeps it refused, in the prepaid wording; raising the credit limit lets it call again", async () => {
    // The tenant is where the case above left it: postpaid, its balance at
    // or below minus its limit, and refused with the credit-limit message.
    const current = await world!.cp.admin.get(`/admin/tenants/${tenant.id}`);
    expect(current.status, current.text).toBe(200);
    const prepaid = await world!.cp.admin.patch(
      `/admin/tenants/${tenant.id}`,
      { billing_mode: "prepaid" },
      current.version,
    );
    expect(prepaid.status, prepaid.text).toBe(200);
    expect(prepaid.body.billing_mode, prepaid.text).toBe("prepaid");
    // The credit limit is an overdraft in prepaid too, so the money left is
    // still the balance plus the limit, and it is still none.
    const held = await account();
    expect(held.billing_mode).toBe("prepaid");
    expect(held.credit_limit_micro_cny).toBe(LIMIT_MICRO_USD);
    expect(held.available_micro_cny).toBe(held.balance_micro_cny + LIMIT_MICRO_USD);
    expect(held.exhausted).toBe(true);
    // The mode change alone rewrites the block: the gateway goes on refusing,
    // now with the prepaid wording.
    const refused = await chatsAnswer(429, "chats to be refused in the prepaid wording", PREPAID_MESSAGE);
    expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");

    // Raising the limit gives a prepaid tenant below 0 money to spend.
    const raised = await world!.cp.admin.patch(`/admin/tenants/${tenant.id}/billing`, { credit_limit_cny: "1.00" });
    expect(raised.status, raised.text).toBe(200);
    // Settled before the answer: the account already says it is not blocked.
    expect(raised.body.billing_mode, raised.text).toBe("prepaid");
    expect(raised.body.credit_limit_micro_cny).toBe(1_000_000);
    expect(raised.body.balance_micro_cny, raised.text).toBeLessThan(0);
    expect(raised.body.available_micro_cny, raised.text).toBe(raised.body.balance_micro_cny + 1_000_000);
    expect(raised.body.exhausted, raised.text).toBe(false);
    await chatsAnswer(200, "chats to be served after the credit limit was raised");
  });
});
