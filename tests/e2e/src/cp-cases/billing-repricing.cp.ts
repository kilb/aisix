import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  billingWorld,
  CALL_MICRO_USD,
  CHAT_CARD,
  CHAT_MODEL,
  type BillingTenant,
  type BillingWorld,
} from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway } from "../harness/cp-upstream.js";

// E2E: an operator corrects past charges with a re-pricing run. Usage on a
// model nobody priced yet is stored unpriced; once the model is priced, a
// previewed run charges it at the current price, and an admin's apply moves
// the tenant's balance and shows the correction on each call, once. A run
// over a price that went down credits the tenant. Real calls go through the
// real gateway, and every step through the operator and tenant APIs; only
// the upstream model is a stand-in.

const LATE_MODEL = "chat-later";
const OWNER_PASSWORD = "repricing-e2e-owner-7c1";
const REASON = "补录漏配的价格 7c1";

describe("re-pricing corrects past charges with ledger adjustments", () => {
  let world: BillingWorld | undefined;
  let lateModel: Json;
  let tenant: BillingTenant;
  let owner: CpPortal;

  const chat = async (model: string): Promise<void> => {
    const answer = await callGateway(world!.gateway.proxyUrl, tenant.key, "/v1/chat/completions", {
      model,
      messages: [{ role: "user", content: "what will this cost?" }],
    });
    expect(answer.status, answer.text).toBe(200);
  };

  /** The owner's calls on `model`, once `count` are listed and `ready` holds for each. */
  const callsOf = (model: string, count: number, ready: (c: Json) => boolean): Promise<Json[]> => {
    let last = "none read";
    return pollUntil(
      `${count} calls on ${model} in the call log`,
      async () => {
        const res = await owner.get("/api/usage/calls");
        expect(res.status, res.text).toBe(200);
        const calls = (res.body.items as Json[]).filter((c) => c.model.name === model);
        last = JSON.stringify(calls.map((c) => c.charge));
        return calls.length === count && calls.every(ready) ? calls : undefined;
      },
      { timeoutMs: 45_000, intervalMs: 500, describe: () => last },
    );
  };

  const balance = async (): Promise<number> => {
    const res = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
    expect(res.status, res.text).toBe(200);
    return res.body.balance_micro_usd as number;
  };

  const preview = async (body: Json): Promise<Json> => {
    const res = await world!.cp.admin.post("/admin/billing/repricing", { ...body, reason: REASON });
    expect(res.status, res.text).toBe(201);
    return res.body;
  };

  const apply = (id: string) => world!.cp.admin.post(`/admin/billing/repricing/${id}/apply`, {});

  const price = async (modelId: string, card: Json): Promise<void> => {
    const res = await world!.cp.admin.post("/admin/pricing/sell", {
      model_id: modelId,
      effective_from: new Date().toISOString(),
      card,
    });
    expect(res.status, res.text).toBe(201);
  };

  beforeAll(async () => {
    world = await billingWorld();
    const w = world;
    // A model on the world's upstream account that nobody has priced.
    lateModel = await w.cp.admin.create("models", {
      display_name: LATE_MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: w.models.chat.doc.provider_key_id,
    });
    tenant = await w.tenant("Repricing E2E Co", {
      setup: async (id) => {
        const current = await w.cp.admin.get(`/admin/tenants/${id}`);
        const granted = await w.cp.admin.patch(
          `/admin/tenants/${id}`,
          { allowed_model_ids: [w.models.chat.id, w.models.embed.id, lateModel.id] },
          current.version,
        );
        expect(granted.status, granted.text).toBe(200);
        await fundTenant(w.cp, id, "1.00");
      },
    });
    const invite = await w.cp.admin.post(`/admin/tenants/${tenant.id}/invites`, {
      email: "owner@repricing-e2e.test",
      role: "owner",
    });
    expect(invite.status, invite.text).toBe(201);
    owner = w.cp.portal();
    const accepted = await owner.post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: OWNER_PASSWORD,
      display_name: "Repricing Owner",
    });
    expect(accepted.status, accepted.text).toBe(201);
  });

  afterAll(async () => {
    await world?.close();
  });

  test("Usage on an unpriced model is charged by a re-pricing run, once", async () => {
    const from = new Date(Date.now() - 60_000).toISOString();
    for (let i = 0; i < 3; i++) await chat(LATE_MODEL);
    await callsOf(LATE_MODEL, 3, (c) => c.charge.status === "pending");
    const before = await balance();

    await price(lateModel.id, CHAT_CARD);
    const run = await preview({ from, to: new Date().toISOString(), model_id: lateModel.id });
    expect(run.status).toBe("previewed");
    expect(run.events).toBe(3);
    expect(run.changed).toBe(3);
    expect(run.delta_micro_usd).toBe(3 * CALL_MICRO_USD);
    expect(run.by_tenant.map((t: Json) => [t.tenant_id, t.delta_micro_usd])).toEqual([
      [tenant.id, 3 * CALL_MICRO_USD],
    ]);
    // A preview moves no money.
    expect(await balance()).toBe(before);

    // The stack's operator is an admin.
    const applied = await apply(run.id);
    expect(applied.status, applied.text).toBe(200);
    expect(applied.body.status).toBe("applied");
    expect(await balance()).toBe(before - 3 * CALL_MICRO_USD);
    const calls = await callsOf(LATE_MODEL, 3, () => true);
    for (const c of calls) {
      expect(c.charge.status, JSON.stringify(c)).toBe("charged");
      expect(c.charge.adjusted_micro_usd, JSON.stringify(c)).toBe(CALL_MICRO_USD);
    }

    const ledger = await owner.get("/api/billing/ledger?kind=adjustment");
    expect(ledger.status, ledger.text).toBe(200);
    expect(ledger.body.items.map((e: Json) => [e.amount_micro_usd, e.repricing_run_id])).toEqual([
      [-3 * CALL_MICRO_USD, run.id],
    ]);

    const again = await apply(run.id);
    expect(again.status, again.text).toBe(409);
    expect(again.body.code).toBe("run_state");
    expect(await balance()).toBe(before - 3 * CALL_MICRO_USD);
  });

  test("A run that lowers a price credits the tenant", async () => {
    const from = new Date(Date.now() - 60_000).toISOString();
    for (let i = 0; i < 2; i++) await chat(CHAT_MODEL);
    await callsOf(CHAT_MODEL, 2, (c) => c.charge.status === "charged");
    const before = await balance();

    // Half the price: 1000 × 3 + 200 × 10 = 5 000 µ$ a call.
    await price(world!.models.chat.id, { default: { base: { input: "3", output: "10" } } });
    const run = await preview({
      from,
      to: new Date().toISOString(),
      model_id: world!.models.chat.id,
      tenant_id: tenant.id,
    });
    expect(run.changed).toBe(2);
    expect(run.delta_micro_usd).toBe(-CALL_MICRO_USD);
    const applied = await apply(run.id);
    expect(applied.status, applied.text).toBe(200);
    expect(await balance()).toBe(before + CALL_MICRO_USD);
    const calls = await callsOf(CHAT_MODEL, 2, () => true);
    for (const c of calls) {
      expect(c.charge.amount_micro_usd, JSON.stringify(c)).toBe(CALL_MICRO_USD);
      expect(c.charge.adjusted_micro_usd, JSON.stringify(c)).toBe(-CALL_MICRO_USD / 2);
    }
  });
});
