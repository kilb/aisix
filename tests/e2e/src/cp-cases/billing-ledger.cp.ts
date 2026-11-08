import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_USD, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway } from "../harness/cp-upstream.js";

// E2E: a tenant's owner reads what its calls cost, entry by entry and per
// day, through the real control plane. The calls go through a real gateway;
// only the upstream model is a stand-in.

const OWNER = { email: "owner@ledger-e2e.test", password: "ledger-e2e-owner-5d2" };
/** How long a charge may take to reach the ledger. */
const WITHIN_MS = 15_000;

describe("a tenant reads its ledger and its daily totals", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;
  let owner: CpPortal;

  /** The ledger, oldest first, once it holds `count` entries. */
  const entries = (count: number): Promise<Json[]> => {
    let last = "no read";
    return pollUntil(
      `the ledger to hold ${count} entries`,
      async () => {
        const res = await owner.get("/api/billing/ledger");
        last = `${res.status} ${res.text}`;
        const items = res.status === 200 ? (res.body.items as Json[]) : [];
        return items.length >= count ? [...items].reverse() : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250, describe: () => last },
    );
  };

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Ledger E2E Co", {
      setup: (id) => fundTenant(world!.cp, id, "1.00"),
    });
    const invite = await world.cp.admin.post(`/admin/tenants/${tenant.id}/invites`, {
      email: OWNER.email,
      role: "owner",
    });
    expect(invite.status, invite.text).toBe(201);
    owner = world.cp.portal();
    const accepted = await owner.post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: OWNER.password,
    });
    expect(accepted.status, accepted.text).toBe(201);
  });

  afterAll(async () => {
    await world?.close();
  });

  test("The ledger shows each debit with the balance after it", async () => {
    for (let call = 0; call < 2; call++) {
      const answer = await callGateway(world!.gateway.proxyUrl, tenant.key, "/v1/chat/completions", {
        model: CHAT_MODEL,
        messages: [{ role: "user", content: "spend a little" }],
      });
      expect(answer.status, answer.text).toBe(200);
    }
    const items = await entries(2);
    expect(
      items.map((i) => [i.kind, i.amount_micro_usd, i.balance_after_micro_usd, i.model?.name]),
    ).toEqual([
      ["usage", -CALL_MICRO_USD, -CALL_MICRO_USD, CHAT_MODEL],
      ["usage", -CALL_MICRO_USD, -2 * CALL_MICRO_USD, CHAT_MODEL],
    ]);
  });

  test("Today's summary adds them up", async () => {
    // The day the debits were posted on, read from the ledger itself, so a
    // run across 00:00 UTC still asks for the right day.
    const posted = await entries(2);
    const day = new Date(posted[0].posted_at);
    day.setUTCHours(0, 0, 0, 0);
    const from = day.toISOString();
    const to = new Date(day.getTime() + 86_400_000).toISOString();
    const res = await owner.get(`/api/billing/summary?group=day&from=${from}&to=${to}`);
    expect(res.status, res.text).toBe(200);
    // Both debits are in the one period when they share a day; when the run
    // crossed midnight the first day holds only the first, and the totals
    // still have to match the entries of that day.
    const sameDay = posted.filter((e) => Date.parse(e.posted_at) < day.getTime() + 86_400_000);
    expect(res.body.items).toHaveLength(1);
    const row = res.body.items[0];
    expect(Date.parse(row.period_start)).toBe(day.getTime());
    expect(row.usage_micro_usd).toBe(-sameDay.length * CALL_MICRO_USD);
    expect(row.closing_balance_micro_usd).toBe(-sameDay.length * CALL_MICRO_USD);
  });
});
