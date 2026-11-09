import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: no money surface of this release names another currency, and no tenant
// answer names the routing identity behind its group. Every amount the
// control plane and the gateway answer with is CNY, under a neutral name: a
// field called `_usd` that carries CNY is a renamed unit, not a renamed field,
// and it is what a client would read as dollars.
//
// The routing name `grp-route-9f3` is an operator's identifier: a tenant is
// shown its group's display name and never the name a provider key matches.
//
// Only the upstream model is a stand-in.

/** The routing name no tenant answer may carry, and the display name it shows instead. */
const ROUTING_NAME = "grp-route-9f3";
const DISPLAY_NAME = "账单 9f3";
const PASSWORD = "no-usd-e2e-pass-9f3";

describe("no answer names the dollar, and no tenant answer names a routing group", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;
  let owner: CpPortal;
  /** The tenant's key in the routing-name group, and its id. */
  let groupKey: string;
  let groupKeyId: string;
  /** The case's own request, which the gateway's lookup is asked about. */
  let requestId: string;

  async function seat(email: string, role: string): Promise<{ portal: CpPortal; id: string }> {
    const invite = await world!.cp.admin.post(`/admin/tenants/${tenant.id}/invites`, { email, role });
    expect(invite.status, invite.text).toBe(201);
    const accepted = await world!.cp.portal().post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: PASSWORD,
    });
    expect(accepted.status, accepted.text).toBe(201);
    const portal = world!.cp.portal();
    const login = await portal.login(email, PASSWORD);
    expect(login.status, login.text).toBe(200);
    const members = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/members`);
    const id = (members.body.items as Json[]).find((m) => m.email === email).id as string;
    return { portal, id };
  }

  beforeAll(async () => {
    world = await billingWorld();
    const w = world;

    // The group the tenant's key names, with the routing name it must never be
    // shown. The billing world's account serves both groups, so a key of
    // either reaches the same model and the charge carries the key's group.
    const group = await w.cp.admin.createGroup(ROUTING_NAME, DISPLAY_NAME);
    const account = w.models.chat.doc.provider_key_id as string;
    const current = await w.cp.admin.get(`/admin/resources/provider_keys/${account}`);
    expect(current.status, current.text).toBe(200);
    const shared = await w.cp.admin.put(
      `/admin/resources/provider_keys/${account}`,
      { doc: { ...current.body.doc, groups: ["shared", ROUTING_NAME] } },
      current.version,
    );
    expect(shared.status, shared.text).toBe(200);

    // The tenant, assigned `shared` and the routing-name group, with a key in
    // each: a key names exactly one group, so the second one is the only key
    // that group is shown behind.
    const created = await w.cp.admin.post("/admin/tenants", { name: "No Usd E2E Co" });
    expect(created.status, created.text).toBe(201);
    const tenantId = created.body.id as string;
    const granted = await w.cp.admin.patch(
      `/admin/tenants/${tenantId}`,
      {
        allowed_model_ids: [w.models.chat.id, w.models.embed.id],
        allowed_provider_groups: ["shared", ROUTING_NAME],
      },
      created.body.version,
    );
    expect(granted.status, granted.text).toBe(200);
    await fundTenant(w.cp, tenantId, "1.00");
    const groups = (await w.cp.admin.get("/admin/provider-groups")).body.items as Json[];
    const groupId = (name: string): string => {
      const group = groups.find((g) => g.name === name);
      if (!group) throw new Error(`no group ${name}`);
      return group.id as string;
    };
    const minted = await w.cp.admin.post(`/admin/tenants/${tenantId}/keys`, {
      label: "no-usd-shared-9f3",
      group_id: groupId("shared"),
    });
    expect(minted.status, minted.text).toBe(201);
    tenant = { id: tenantId, key: minted.body.plaintext as string, keyId: minted.body.key.id as string };
    const inGroup = await w.cp.admin.post(`/admin/tenants/${tenantId}/keys`, {
      label: "no-usd-in-9f3",
      group_id: group.id as string,
    });
    expect(inGroup.status, inGroup.text).toBe(201);
    groupKey = inGroup.body.plaintext as string;
    groupKeyId = inGroup.body.key.id as string;
    const ownerSeat = await seat("owner@no-usd-e2e.test", "owner");
    owner = ownerSeat.portal;
    for (const key of [tenant.key, groupKey]) {
      const proxy = new ProxyClient(w.gateway.proxyUrl, key);
      await pollUntil("the caller key to authenticate", async () =>
        (await proxy.listModels()).status === 200 ? true : undefined,
      );
    }

    // One call under a known request id, so the gateway's own lookups have a
    // charge to report.
    requestId = "req-nousd-9f3";
    const answer = await callGateway(
      w.gateway.proxyUrl,
      tenant.key,
      "/v1/chat/completions",
      { model: CHAT_MODEL, messages: [{ role: "user", content: "price this" }] },
      { "x-aisix-request-id": requestId },
    );
    expect(answer.status, answer.text).toBe(200);
    let last = "no lookup";
    await pollUntil(
      "the charge to reach the request lookup",
      async () => {
        const lookup = await callGateway(
          world!.gateway.proxyUrl,
          tenant.key,
          `/v1/generation?id=${encodeURIComponent(requestId)}`,
          undefined,
        );
        last = `${lookup.status} ${lookup.text}`;
        return lookup.status === 200 ? true : undefined;
      },
      { timeoutMs: 30_000, intervalMs: 500, describe: () => last },
    );
  });

  afterAll(async () => {
    await world?.close();
  });

  test("No /api, /dp-relayed or gateway client answer carries a _usd field, and none on /api carries cost_micro_usd", async () => {
    const w = world!;
    const keyId = tenant.keyId;
    const budget = await owner.post("/api/budgets", {
      scope: "api_key",
      scope_ref: keyId,
      kind: "periodic",
      period: "day",
      limit_cny: "0.50",
    });
    expect(budget.status, budget.text).toBe(201);

    // Every `/api` answer a tenant reads money from, plus the model listing.
    const answers: [string, { status: number; text: string }][] = [
      ["/api/budgets", await owner.get("/api/budgets")],
      [`/api/keys/${keyId}/quota`, await owner.get(`/api/keys/${keyId}/quota`)],
      ["/api/usage/calls", await owner.get("/api/usage/calls")],
      ["/api/usage/summary?group=day", await owner.get(`/api/usage/summary?${window()}`)],
      ["/api/prices", await owner.get("/api/prices")],
      ["/api/groups", await owner.get("/api/groups")],
      ["/api/keys", await owner.get("/api/keys")],
    ];
    for (const [path, answer] of answers) {
      expect(answer.status, `${path}: ${answer.text}`).toBe(200);
      expect(answer.text, path).not.toContain("_usd");
      expect(answer.text, path).not.toContain("cost_micro_usd");
    }

    // The gateway's `/dp/v1` answers, as its client routes relay them: a
    // figure under an old name would reach a caller exactly like this.
    for (const [path, key] of [
      ["/v1/key", tenant.key],
      [`/v1/generation?id=${encodeURIComponent(requestId)}`, tenant.key],
      ["/v1/models", tenant.key],
    ] as const) {
      const answer: GatewayAnswer = await callGateway(w.gateway.proxyUrl, key, path, undefined);
      expect(answer.status, `${path}: ${answer.text}`).toBe(200);
      expect(answer.text, path).not.toContain("_usd");
      expect(answer.text, path).not.toContain("cost_micro_usd");
    }
  });

  test("The routing name grp-route-9f3 appears in no /api answer, while its display name does", async () => {
    const w = world!;
    // The group's own page, and a call through the key that names it: the
    // display name is what a tenant is shown, and the call log carries it.
    const groups = await owner.get("/api/groups");
    expect(groups.status, groups.text).toBe(200);
    expect(groups.text, groups.text).toContain(DISPLAY_NAME);

    const chat = await callGateway(w.gateway.proxyUrl, groupKey, "/v1/chat/completions", {
      model: CHAT_MODEL,
      messages: [{ role: "user", content: "whose group is this" }],
    });
    expect(chat.status, chat.text).toBe(200);
    let last = "no call log";
    const call = await pollUntil(
      "the call to reach the call log",
      async () => {
        const answer = await owner.get(`/api/usage/calls?api_key_id=${groupKeyId}`);
        last = answer.text;
        const items = ((answer.status === 200 ? answer.body.items : []) as Json[]) ?? [];
        return answer.status === 200 && items.length > 0 ? items[0] : undefined;
      },
      { timeoutMs: 30_000, intervalMs: 500, describe: () => last },
    );
    expect((call.charge as Json)?.group, JSON.stringify(call)).toMatchObject({
      display_name: DISPLAY_NAME,
    });

    for (const path of [
      "/api/groups",
      "/api/keys",
      "/api/prices",
      "/api/usage/calls",
      `/api/usage/calls?api_key_id=${groupKeyId}`,
    ]) {
      const answer = await owner.get(path);
      expect(answer.status, `${path}: ${answer.text}`).toBe(200);
      expect(answer.text, path).not.toContain(ROUTING_NAME);
    }

    // The routing name is an operator's identifier: `/admin` names it, and no
    // `/api` answer does.
    const admin = await w.cp.admin.get(`/admin/provider-groups`);
    expect(admin.status, admin.text).toBe(200);
    expect(admin.text, "the operator's own view names it").toContain(ROUTING_NAME);
  });
});

/** `from`/`to` around now, for a summary of what just happened. */
function window(): string {
  const day = 24 * 3600 * 1000;
  const iso = (ms: number) => new Date(ms).toISOString().replace(/\.\d+Z$/, "Z");
  return `group=day&from=${iso(Date.now() - day)}&to=${iso(Date.now() + day)}`;
}
