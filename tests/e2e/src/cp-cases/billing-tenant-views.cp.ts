import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, startPlatformUpstream, type PlatformUpstream } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: a tenant reads its own calls and usage across both planes, and learns
// nothing of the platform's channel from them. Real calls go through the real
// gateway; the control plane prices them and its `/api/usage/*` routes answer
// them. Only the upstream model is a stand-in.

/** What the channel is called, which no tenant answer may carry. */
const CHANNEL_TOKENS = ["acct-bill-7c1", "grp-bill-5d2", "upstream-mdl-9f3", "upstream secret 4b7"];
const REQUEST_ID = "req-tv-7c1";
const PASSWORD = "billing-views-e2e-pass-7c1";

const chatBody = (model: string, stream = false) => ({
  model,
  stream,
  ...(stream ? { stream_options: { include_usage: true } } : {}),
  messages: [{ role: "user", content: "what did that cost?" }],
});

/** `from`/`to` around now, for a summary of what just happened. */
function window(): string {
  const day = 24 * 3600 * 1000;
  const iso = (ms: number) => new Date(ms).toISOString().replace(/\.\d+Z$/, "Z");
  return `group=day&from=${iso(Date.now() - day)}&to=${iso(Date.now() + day)}`;
}

describe("a tenant's call log and usage summary carry its calls and nothing of the platform's channel", () => {
  let world: BillingWorld | undefined;
  let failing: PlatformUpstream | undefined;
  let busy: PlatformUpstream | undefined;
  let tenant: BillingTenant;
  let owner: CpPortal;
  let member: CpPortal;
  let billing: CpPortal;
  let memberKey: string;
  let memberId: string;
  let other: BillingTenant;
  let otherOwner: CpPortal;
  let hiddenModelId: string;

  const call = (key: string, model: string, headers?: Record<string, string>, stream = false) =>
    callGateway(world!.gateway.proxyUrl, key, "/v1/chat/completions", chatBody(model, stream), headers);

  /** An invited member of `tenantId`, logged in. */
  async function seat(tenantId: string, email: string, role: string): Promise<CpPortal> {
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
    return portal;
  }

  async function calls(portal: CpPortal, query = ""): Promise<Json[]> {
    const res = await portal.get(`/api/usage/calls${query}`);
    expect(res.status, res.text).toBe(200);
    return res.body.items as Json[];
  }

  /** Waits until `portal`'s call log holds `count` calls, and returns them. */
  const callsReach = (portal: CpPortal, count: number, query = ""): Promise<Json[]> => {
    let last = 0;
    return pollUntil(
      `${count} calls in the call log`,
      async () => {
        const items = await calls(portal, query);
        last = items.length;
        return items.length >= count ? items : undefined;
      },
      { timeoutMs: 45_000, intervalMs: 500, describe: () => `${last} so far` },
    );
  };

  beforeAll(async () => {
    world = await billingWorld();
    const w = world;
    failing = await startPlatformUpstream("upstream secret 4b7");
    failing.failWith = 500;
    busy = await startPlatformUpstream("overload-views-e2e");
    busy.failWith = 529;

    // Two models on accounts that carry the channel's names, and one the
    // tenant is not granted.
    const channel = async (name: string, up: PlatformUpstream, model: string) => {
      const account = await w.cp.admin.create("provider_keys", {
        display_name: name,
        provider: "openai",
        api_key: `sk-${name}`,
        api_base: `${up.baseUrl}/v1`,
        groups: ["shared", "grp-bill-5d2"],
      });
      return w.cp.admin.create("models", {
        display_name: model,
        provider: "openai",
        model_name: "upstream-mdl-9f3",
        provider_key_id: account.id,
      });
    };
    const fail = await channel("acct-bill-7c1", failing, "chat-bill-fail");
    const slow = await channel("acct-bill-7c1-busy", busy, "chat-bill-busy");
    const hidden = await w.cp.admin.create("models", {
      display_name: "chat-hidden",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: w.models.chat.doc.provider_key_id,
    });
    hiddenModelId = hidden.id;
    for (const model of [fail, slow, hidden]) {
      const priced = await w.cp.admin.post("/admin/pricing/baselines", {
        model_id: model.id,
        effective_from: new Date().toISOString(),
        card: { default: { base: { input: "6", output: "20" } } },
        note: "the views case's card",
      });
      expect(priced.status, priced.text).toBe(201);
    }

    tenant = await w.tenant("Views E2E Co", {
      setup: async (id) => {
        const current = await w.cp.admin.get(`/admin/tenants/${id}`);
        const granted = await w.cp.admin.patch(
          `/admin/tenants/${id}`,
          { allowed_model_ids: [w.models.chat.id, w.models.embed.id, fail.id, slow.id] },
          current.version,
        );
        expect(granted.status, granted.text).toBe(200);
        await fundTenant(w.cp, id, "1.00");
      },
    });
    other = await w.tenant("Views Other Co", { setup: (id) => fundTenant(w.cp, id, "1.00") });

    owner = await seat(tenant.id, "owner@views-e2e.test", "owner");
    member = await seat(tenant.id, "member@views-e2e.test", "member");
    billing = await seat(tenant.id, "billing@views-e2e.test", "billing");
    otherOwner = await seat(other.id, "owner@views-other-e2e.test", "owner");

    const members = await w.cp.admin.get(`/admin/tenants/${tenant.id}/members`);
    memberId = (members.body.items as Json[]).find((m) => m.email === "member@views-e2e.test").id;
    const minted = await w.cp.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: "member key", member_id: memberId });
    expect(minted.status, minted.text).toBe(201);
    memberKey = minted.body.plaintext;
    const proxy = new ProxyClient(w.gateway.proxyUrl, memberKey);
    await pollUntil("the member's key to authenticate", async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
  });

  afterAll(async () => {
    await world?.close();
    await failing?.close();
    await busy?.close();
  });

  test("The owner's call log holds its seven calls and neither it nor the summary names the channel", async () => {
    await call(tenant.key, CHAT_MODEL, undefined, true);
    await call(tenant.key, CHAT_MODEL);
    await call(tenant.key, "chat-bill-fail");
    await call(tenant.key, "chat-bill-busy");
    await call(tenant.key, "chat-hidden");
    await call(tenant.key, "no-such-model-3a1");
    await call(tenant.key, CHAT_MODEL, { "x-aisix-request-id": REQUEST_ID });

    const items = await callsReach(owner, 7);
    expect(items.length, JSON.stringify(items)).toBe(7);
    const log = await owner.get("/api/usage/calls");
    const summary = await owner.get(`/api/usage/summary?${window()}`);
    expect(summary.status, summary.text).toBe(200);
    for (const token of CHANNEL_TOKENS) {
      expect(log.text, `${token} in the call log`).not.toContain(token);
      expect(summary.text, `${token} in the summary`).not.toContain(token);
    }
    expect(log.text).not.toContain("cost_usd");
  });

  test("An upstream's own status never reaches the tenant", async () => {
    const items = await calls(owner);
    const busyCall = items.find((c) => c.model.name === "chat-bill-busy");
    expect(busyCall, JSON.stringify(items)).toBeDefined();
    expect(busyCall!.status).toBe("failed");
    expect(busyCall!.status_code).toBeNull();
    expect(busyCall!.error_class).toBe("upstream_unavailable");
    for (const c of items) expect(c.status_code, JSON.stringify(c)).not.toBe(529);
  });

  test("A forbidden model's call reads exactly like an unknown model's", async () => {
    const items = await calls(owner);
    const hidden = items.find((c) => c.model.name === "chat-hidden");
    const unknown = items.find((c) => c.model.name === "no-such-model-3a1");
    expect(hidden, JSON.stringify(items)).toBeDefined();
    expect(unknown, JSON.stringify(items)).toBeDefined();
    expect(hidden!.status).toBe(unknown!.status);
    expect(hidden!.status).toBe("failed");
    expect(hidden!.error_class).toBe(unknown!.error_class);
    expect(hidden!.error_class).toBe("model_not_found");
    expect(hidden!.model.id).toBeNull();
    expect(unknown!.model.id).toBeNull();
    expect(hiddenModelId).toBeDefined();
  });

  test("A call is found by its request id", async () => {
    const found = await calls(owner, `?request_id=${REQUEST_ID}`);
    expect(found.length, JSON.stringify(found)).toBe(1);
    expect(found[0].request_id).toBe(REQUEST_ID);
    expect(found[0].status).toBe("succeeded");
    expect(found[0].status_code).toBe(200);
  });

  test("A member sees only their own key's calls; a billing member sees all of them", async () => {
    await call(memberKey, CHAT_MODEL);
    await call(memberKey, CHAT_MODEL);
    const mine = await callsReach(member, 2);
    expect(mine.length, JSON.stringify(mine)).toBe(2);
    for (const c of mine) expect(c.api_key.label).toBe("member key");
    const everything = await callsReach(billing, 9);
    expect(everything.length).toBe(9);
    const total = (await owner.get(`/api/usage/summary?${window()}`)).body.items.reduce(
      (n: number, row: Json) => n + row.requests,
      0,
    );
    expect(total).toBe(9);
    const own = (await member.get(`/api/usage/summary?${window()}`)).body.items.reduce(
      (n: number, row: Json) => n + row.requests,
      0,
    );
    expect(own).toBe(2);
  });

  test("Another tenant's calls never appear", async () => {
    await call(other.key, CHAT_MODEL);
    const theirs = await callsReach(otherOwner, 1);
    expect(theirs.length).toBe(1);
    const ours = await calls(owner);
    const ids = new Set(ours.map((c) => c.event_id));
    expect(ids.has(theirs[0].event_id)).toBe(false);
    expect(ours.length).toBe(9);
  });
});
