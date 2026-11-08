import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: an owner's member quota and a member's own key caps block at the
// gateway, across both planes. Everything is driven through the tenant
// portal API and the real gateway; only the upstream model is a stand-in.
// Each call costs 10 000 µ$ ($0.01).

/** How long a change may take to reach the gateway. */
const WITHIN_MS = 15_000;
const PASSWORD = "quotas-e2e-pass-7c1";

describe("a tenant's own levels cap members and keys at the gateway", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;
  let owner: CpPortal;
  let member: CpPortal;
  let memberId: string;
  let k1: string;
  let k1Id: string;
  let k2: string;
  let k2Id: string;
  let k3: string;
  let k3Id: string;
  let k1Cap: string;
  // The clock before K1's cap was created: the mark's period is that day's or the next.
  let capCreatedAt: Date;

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
  const served = async (key: string): Promise<void> => {
    const answer = await chat(key);
    expect(answer.status, answer.text).toBe(200);
  };

  async function seat(email: string, role: string, tenantId = tenant.id): Promise<{ portal: CpPortal; id: string }> {
    const invite = await world!.cp.admin.post(`/admin/tenants/${tenantId}/invites`, { email, role });
    expect(invite.status, invite.text).toBe(201);
    const accepted = await world!.cp.portal().post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: PASSWORD,
    });
    expect(accepted.status, accepted.text).toBe(201);
    const portal = world!.cp.portal();
    const login = await portal.login(email, PASSWORD);
    expect(login.status, login.text).toBe(200);
    const members = await world!.cp.admin.get(`/admin/tenants/${tenantId}/members`);
    const id = (members.body.items as Json[]).find((m) => m.email === email).id as string;
    return { portal, id };
  }

  async function mint(label: string, memberId: string, tenantId = tenant.id): Promise<{ plaintext: string; id: string }> {
    const minted = await world!.cp.admin.post(`/admin/tenants/${tenantId}/keys`, { label, member_id: memberId });
    expect(minted.status, minted.text).toBe(201);
    const proxy = new ProxyClient(world!.gateway.proxyUrl, minted.body.plaintext);
    await pollUntil(`${label} to authenticate`, async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
    return { plaintext: minted.body.plaintext, id: minted.body.key.id };
  }

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Quotas E2E Co", {
      billingMode: "postpaid",
      setup: (id) => fundTenant(world!.cp, id, "1.00"),
    });
    const o = await seat("owner@quotas-e2e.test", "owner");
    owner = o.portal;
    const m = await seat("member@quotas-e2e.test", "member");
    member = m.portal;
    memberId = m.id;
    ({ plaintext: k1, id: k1Id } = await mint("k1", memberId));
    ({ plaintext: k2, id: k2Id } = await mint("k2", memberId));
    ({ plaintext: k3, id: k3Id } = await mint("k3", o.id));
  });

  afterAll(async () => {
    await world?.close();
  });

  test("An owner's member quota blocks both of that member's keys and not the owner's", async () => {
    const created = await owner.post("/api/budgets", { scope: "member", scope_ref: memberId, kind: "periodic", period: "day", limit_usd: "0.05" });
    expect(created.status, created.text).toBe(201);
    expect(created.body.set_by).toBe("org_admin");

    // Five calls through K2 spend the $0.05; each is served, since the quota
    // still has money when it is sent.
    for (let i = 0; i < 5; i++) await served(k2);
    for (const [name, key] of [["K1", k1], ["K2", k2]] as const) {
      const refused = await chatsAnswer(key, 429, `${name} to be refused at the member quota`);
      expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
      expect(refused.json?.error?.scope, refused.text).toBe("member");
    }
    await served(k3);

    // Lifted again, so the key caps below meet no member quota.
    const removed = await owner.del(`/api/budgets/${created.body.id}`);
    expect(removed.status, removed.text).toBe(204);
    await chatsAnswer(k1, 200, "K1 to be served once the member quota is gone");
  });

  test("A member's own key cap blocks that key only", async () => {
    capCreatedAt = new Date();
    const cap = await member.post("/api/budgets", { scope: "api_key", scope_ref: k1Id, kind: "periodic", period: "day", limit_usd: "0.02" });
    expect(cap.status, cap.text).toBe(201);
    expect(cap.body.set_by).toBe("member");
    k1Cap = cap.body.id;
    // The call served above spent $0.01 of K1's day, and one more reaches it.
    await served(k1);
    const refused = await chatsAnswer(k1, 429, "K1 to be refused at its cap");
    expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
    expect(refused.json?.error?.scope, refused.text).toBe("api_key");
    expect(refused.json?.error?.period, refused.text).toBe("day");
    await served(k2);
    await served(k3);
  });

  test("A cumulative cap blocks with no reset time", async () => {
    const cap = await member.post("/api/budgets", { scope: "api_key", scope_ref: k2Id, kind: "cumulative", limit_usd: "0.01" });
    expect(cap.status, cap.text).toBe(201);
    await served(k2);
    const refused = await chatsAnswer(k2, 429, "K2 to be refused at its cumulative cap");
    expect(refused.json?.error?.period, refused.text).toBe("total");
    expect(refused.json?.error?.period_resets_at, refused.text).toBeUndefined();
    expect(refused.headers.get("retry-after")).toBeNull();

    const raised = await member.patch(`/api/budgets/${cap.body.id}`, { limit_usd: "1.00" });
    expect(raised.status, raised.text).toBe(200);
    await chatsAnswer(k2, 200, "K2 to be served after the cap was raised");
  });

  test("A refusal states the cap's figures exactly as the key's own lookup does", async () => {
    // A tenant with a 20 % discount pays $0.008 a call, so its spend is not
    // whole cents. Its member caps key KX at $0.01 in total, which the second
    // call passes, at $0.016.
    const exact = await world!.tenant("Quotas Exact Co", {
      setup: async (id) => {
        await fundTenant(world!.cp, id, "1.00");
        const discount = await world!.cp.admin.post("/admin/pricing/discounts", {
          tenant_id: id,
          effective_from: new Date().toISOString(),
          discount_bps: 2000,
        });
        expect(discount.status, discount.text).toBe(201);
      },
    });
    const holder = await seat("member@quotas-exact-e2e.test", "member", exact.id);
    const kx = await mint("kx", holder.id, exact.id);
    const cap = await holder.portal.post("/api/budgets", { scope: "api_key", scope_ref: kx.id, kind: "cumulative", limit_usd: "0.01" });
    expect(cap.status, cap.text).toBe(201);
    await served(kx.plaintext);
    await served(kx.plaintext);

    // Nothing more is sent until the gateway holds the cap's mark, so the
    // spend stays at the two calls.
    let last = "no read";
    await pollUntil(
      "KX's cap to carry its mark at the gateway",
      async () => {
        const res = await world!.cp.admin.get(`/admin/resources/budgets/${cap.body.id}`);
        last = res.text;
        return res.status === 200 && res.body.doc?.exhausted && res.body.rollout?.state === "applied" ? true : undefined;
      },
      { timeoutMs: 30_000, intervalMs: 250, describe: () => last },
    );
    const refused = await chat(kx.plaintext);
    expect(refused.status, refused.text).toBe(429);
    expect(refused.json?.error?.code, refused.text).toBe("budget_exceeded");
    expect(refused.json?.error?.scope, refused.text).toBe("api_key");

    const lookup = await callGateway(world!.gateway.proxyUrl, kx.plaintext, "/v1/key", undefined);
    expect(lookup.status, lookup.text).toBe(200);
    const keyCap = (lookup.json.data.budgets as Json[]).find((b) => b.scope === "api_key");
    expect(keyCap?.spend_usd, lookup.text).toBe("0.016000");
    expect(refused.json.error.spent_usd, refused.text).toBe(keyCap.spend_usd);
    expect(refused.json.error.limit_usd, refused.text).toBe(keyCap.limit_usd);
  });

  test("A member cannot touch what an owner set", async () => {
    const ownerCap = await owner.post("/api/budgets", { scope: "api_key", scope_ref: k1Id, limit_usd: "0.50" });
    expect(ownerCap.status, ownerCap.text).toBe(201);
    const deleted = await member.del(`/api/budgets/${ownerCap.body.id}`);
    expect(deleted.status, deleted.text).toBe(403);
    expect(deleted.body.code).toBe("budget_managed");
    const posted = await member.post("/api/budgets", { scope: "api_key", scope_ref: k3Id, limit_usd: "0.50" });
    expect(posted.status, posted.text).toBe(403);
    expect(posted.body.code).toBe("forbidden");
    // The owner's cap is still there.
    const list = await member.get("/api/budgets");
    expect((list.body.items as Json[]).some((b) => b.id === ownerCap.body.id)).toBe(true);
  });

  test("The mark holds for the current UTC period", async () => {
    const midnightOf = (t: Date): string => {
      const m = new Date(t);
      m.setUTCHours(0, 0, 0, 0);
      return m.toISOString().replace(".000Z", "Z");
    };
    let last = "no read";
    const doc = await pollUntil(
      "the K1 cap's document to carry its mark",
      async () => {
        const res = await world!.cp.admin.get(`/admin/resources/budgets/${k1Cap}`);
        last = res.text;
        return res.status === 200 && res.body.doc?.exhausted ? res.body.doc : undefined;
      },
      { timeoutMs: WITHIN_MS, intervalMs: 250, describe: () => last },
    );
    // Either day's midnight, when the run crossed 00:00 UTC.
    expect([midnightOf(capCreatedAt), midnightOf(new Date())]).toContain(doc.exhausted.period_start);
  });
});
