import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CALL_MICRO_CNY, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { pollUntil, type CpPortal } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";

// E2E: how fast money blocks, across both planes (spec §7.4, §10). A prepaid
// tenant tops up one call's worth at a time and an operator approves it;
// from the answer to the call that spends it, the gateway refuses the
// tenant within 2 s at p95. Everything is driven through the tenant and
// operator APIs and the real gateway; only the upstream model is a stand-in.

const TRIALS = 20;
/** One chat every 100 ms stays under the tenant's default 20 requests/s. */
const POLL_MS = 100;
/** The spec's bound on the block, at p95. */
const BLOCK_WITHIN_MS = 2_000;
/** How long one trial, from the approval to the refusal, may take. */
const TRIAL_MS = 60_000;
const OWNER_PASSWORD = "block-latency-e2e-owner-3f8";

/** `micro` µ$, a whole number of cents, as the dollars a person types. */
const dollars = (micro: number): string => {
  const cents = micro / 10_000;
  return `${Math.floor(cents / 100)}.${String(cents % 100).padStart(2, "0")}`;
};

describe("money blocks a tenant at the gateway within 2 s", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;
  let owner: CpPortal;

  /** One chat, and the moment its answer was read whole. */
  const chat = async (): Promise<{ answer: GatewayAnswer; at: number }> => {
    const answer = await callGateway(world!.gateway.proxyUrl, tenant.key, "/v1/chat/completions", {
      model: CHAT_MODEL,
      messages: [{ role: "user", content: "spend one call" }],
    });
    return { answer, at: performance.now() };
  };
  /**
   * Whether `answer` is the gateway refusing the tenant for money. Any answer
   * other than that and a 200 fails the trial: a rate-limit 429 must never
   * pass for the block.
   */
  const blocked = (answer: GatewayAnswer, trial: number): boolean => {
    if (answer.status === 200) return false;
    if (answer.status === 429 && answer.json?.error?.code === "budget_exceeded") return true;
    throw new Error(`trial ${trial}: unexpected answer ${answer.status} ${answer.text}`);
  };

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Block Latency E2E Co");
    const invite = await world.cp.admin.post(`/admin/tenants/${tenant.id}/invites`, {
      email: "owner@block-latency-e2e.test",
      role: "owner",
    });
    expect(invite.status, invite.text).toBe(201);
    owner = world.cp.portal();
    const accepted = await owner.post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: OWNER_PASSWORD,
      display_name: "Owner",
    });
    expect(accepted.status, accepted.text).toBe(201);
  });

  afterAll(async () => {
    await world?.close();
  });

  test(
    "From spend crossing the balance to the gateway refusing takes at most 2 s at p95",
    async () => {
      const latencies: number[] = [];
      const overshoots: number[] = [];
      // The calls served past the balance in the previous trial; the next
      // top-up repays them, so every trial starts with one call's worth.
      let k = 0;
      for (let trial = 1; trial <= TRIALS; trial++) {
        const requested = await owner.post("/api/billing/topups", { amount_cny: dollars((1 + k) * CALL_MICRO_CNY) });
        expect(requested.status, requested.text).toBe(201);
        const approved = await world!.cp.admin.post(`/admin/billing/topups/${requested.body.id}/approve`, {});
        expect(approved.status, approved.text).toBe(200);

        // One chat every POLL_MS throughout: refused until the gateway learns
        // of the top-up, then the first 200 (`t0`, the call that spends the
        // balance), then the 200s served past it, then the refusal (`t1`).
        let t0: number | undefined;
        let overshoot = 0;
        let last = "no chat sent";
        const t1 = await pollUntil(
          `trial ${trial}: a chat to be served after the top-up, then refused once it is spent`,
          async () => {
            const { answer, at } = await chat();
            last = `${answer.status} ${answer.text}`;
            if (blocked(answer, trial)) return t0 === undefined ? undefined : at;
            if (t0 === undefined) t0 = at;
            else overshoot++;
            return undefined;
          },
          {
            timeoutMs: TRIAL_MS,
            intervalMs: POLL_MS,
            describe: () =>
              t0 === undefined ? `never served; last answer ${last}` : `${overshoot} calls served past the balance`,
          },
        );
        latencies.push(t1 - t0!);
        overshoots.push(overshoot);
        k = overshoot;
      }

      const sorted = [...latencies].sort((a, b) => a - b);
      // Nearest rank over 20 trials: the 10th and the 19th smallest.
      const p50 = sorted[Math.ceil((TRIALS * 50) / 100) - 1];
      const p95 = sorted[Math.ceil((TRIALS * 95) / 100) - 1];
      const report =
        `block latency over ${TRIALS} trials: p50 ${p50.toFixed(0)} ms, p95 ${p95.toFixed(0)} ms, ` +
        `largest overshoot k ${Math.max(...overshoots)}; ` +
        `latencies ${latencies.map((l) => l.toFixed(0)).join(", ")} ms; overshoots ${overshoots.join(", ")}`;
      console.log(report);
      expect(p95, report).toBeLessThanOrEqual(BLOCK_WITHIN_MS);
    },
    240_000,
  );
});
