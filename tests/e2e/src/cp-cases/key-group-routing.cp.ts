import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  fundTenant,
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { callGateway, startPlatformUpstream, type PlatformUpstream } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: every API key names exactly one provider group, chosen when it is
// created, and its document routes to that group's upstream accounts alone.
//
// Two upstream accounts serve one routing model, each in a group of its own,
// and a tenant is assigned both groups. A key in group A is served only by
// A's account, a key in B only by B's; with A's account down, a key in A gets
// the outage answer though B is healthy. Removing A from the tenant refuses
// the key at the gateway, and assigning it back reopens it.
//
// The platform's own tenant's keys name a group too, and route by it: its key
// in B is served by B's account, and is unaffected while A is down.
//
// Everything is seeded through the control plane's API; the only stand-in is
// the upstream provider.

const ROUTE = "route-gp-e2e";
const ACCOUNT_A = "acct-gp-a";
const ACCOUNT_B = "acct-gp-b";
const GROUP_A = { name: "grp-route-9f3", display: "A 分组 9f3" };
const GROUP_B = { name: "grp-gp-7c1", display: "B 分组 7c1" };

const chat = (model: string) => ({ model, messages: [{ role: "user", content: "hi" }] });

describe("a key routes to the upstream accounts of the one group it names", () => {
  let cp: ControlPlaneStack | undefined;
  let app: ManagedGateway | undefined;
  let upA: PlatformUpstream | undefined;
  let upB: PlatformUpstream | undefined;
  let tenantId: string;
  let accountAId: string;
  let keyAId: string;
  let keyA: string;
  let keyB: string;
  let internalKeyB: string;

  const modelsStatus = async (key: string): Promise<number> =>
    (await new ProxyClient(app!.proxyUrl, key).listModels()).status;

  /** `n` chats through `key`, each answered 200. */
  async function calls(key: string, n: number): Promise<void> {
    for (let i = 0; i < n; i++) {
      const res = await callGateway(app!.proxyUrl, key, "/v1/chat/completions", chat(ROUTE));
      expect(res.status, res.text).toBe(200);
    }
  }

  /** Waits until the admin usage log holds `count` rows for `keyId`; returns them. */
  function logRows(keyId: string, count: number): Promise<Json[]> {
    let seen = 0;
    return pollUntil(
      `${count} stored usage rows for key ${keyId}`,
      async () => {
        const res = await cp!.admin.get(`/admin/usage/events?limit=500&api_key_id=${keyId}`);
        expect(res.status, res.text).toBe(200);
        const rows = res.body.items as Json[];
        seen = rows.length;
        return rows.length >= count ? rows : undefined;
      },
      { timeoutMs: 30_000, intervalMs: 500, describe: () => `${seen} so far` },
    );
  }

  /** An upstream account in `group` and the direct model on it. */
  async function channel(
    label: string,
    up: PlatformUpstream,
    group: string,
  ): Promise<{ accountId: string; modelId: string }> {
    const account = await cp!.admin.create("provider_keys", {
      display_name: label,
      provider: "openai",
      api_key: `sk-${label}-e2e`,
      api_base: `${up.baseUrl}/v1`,
      groups: [group],
    });
    const model = await cp!.admin.create("models", {
      display_name: `${label}-direct`,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: account.id,
    });
    return { accountId: account.id, modelId: model.id };
  }

  beforeAll(async () => {
    upA = await startPlatformUpstream(ACCOUNT_A);
    upB = await startPlatformUpstream(ACCOUNT_B);
    cp = await startControlPlane();
    const gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw);
    await cp.admin.waitRegistered(gw.dpId);

    // The two groups, then the two accounts, one in each, and one routing
    // model over their direct models.
    const groupA = await cp.admin.createGroup(GROUP_A.name, GROUP_A.display);
    const groupB = await cp.admin.createGroup(GROUP_B.name, GROUP_B.display);
    const a = await channel(ACCOUNT_A, upA, GROUP_A.name);
    accountAId = a.accountId;
    const b = await channel(ACCOUNT_B, upB, GROUP_B.name);
    const route = await cp.admin.create("models", {
      display_name: ROUTE,
      routing: {
        strategy: "round_robin",
        targets: [{ model_id: a.modelId }, { model_id: b.modelId }],
      },
    });

    // A tenant assigned both groups, with a key in each: the keys differ only
    // in the group each names at creation.
    const created = await cp.admin.post("/admin/tenants", { name: "Key Group E2E Co" });
    expect(created.status, created.text).toBe(201);
    tenantId = created.body.id;
    const granted = await cp.admin.patch(
      `/admin/tenants/${tenantId}`,
      {
        allowed_model_ids: [route.id],
        allowed_provider_groups: [GROUP_A.name, GROUP_B.name],
      },
      created.body.version,
    );
    expect(granted.status, granted.text).toBe(200);
    await fundTenant(cp, tenantId, "100.00");
    const mintA = await cp.admin.post(`/admin/tenants/${tenantId}/keys`, {
      label: "in-a-e2e",
      group_id: groupA.id,
    });
    expect(mintA.status, mintA.text).toBe(201);
    expect(mintA.body.key.group?.id, mintA.text).toBe(groupA.id);
    expect(mintA.body.key.group?.display_name, mintA.text).toBe(GROUP_A.display);
    expect(mintA.body.key.effective_provider_groups, mintA.text).toEqual([GROUP_A.name]);
    keyAId = mintA.body.key.id;
    keyA = mintA.body.plaintext;
    const mintB = await cp.admin.post(`/admin/tenants/${tenantId}/keys`, {
      label: "in-b-e2e",
      group_id: groupB.id,
    });
    expect(mintB.status, mintB.text).toBe(201);
    expect(mintB.body.key.effective_provider_groups, mintB.text).toEqual([GROUP_B.name]);
    keyB = mintB.body.plaintext;

    // The platform's own tenant, whose keys name any live group: one in B.
    const tenants = await cp.admin.get("/admin/tenants");
    const internalId = (tenants.body.items as Json[]).find((t) => t.is_internal).id;
    const internal = await cp.admin.post(`/admin/tenants/${internalId}/keys`, {
      label: "internal-in-b-e2e",
      allowed_models: ["*"],
      group_id: groupB.id,
    });
    expect(internal.status, internal.text).toBe(201);
    internalKeyB = internal.body.plaintext;

    // The keys are minted last: each authenticating is the readiness gate.
    for (const key of [keyA, keyB, internalKeyB]) {
      const proxy = new ProxyClient(app.proxyUrl, key);
      await pollUntil("the caller key to authenticate", async () =>
        (await proxy.listModels()).status === 200 ? true : undefined,
      );
    }
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await upA?.close();
    await upB?.close();
  });

  test("A key reaches only the upstreams of its group", async () => {
    const before = { a: upA!.requests.length, b: upB!.requests.length };
    await calls(keyA, 10);
    // Every row names the account that served it: A's, never B's.
    const rows = await logRows(keyAId, 10);
    expect(rows.length).toBe(10);
    for (const row of rows) {
      expect(row.event.provider_key_id, JSON.stringify(row)).toBe(accountAId);
    }
    // And only A's stand-in saw any of them.
    expect(upA!.requests.length - before.a).toBe(10);
    expect(upB!.requests.length - before.b).toBe(0);
  });

  test("A key of another group, and the platform's own key in it, reach B's upstream alone", async () => {
    const before = { a: upA!.requests.length, b: upB!.requests.length };
    const throughB = (await cp!.admin.get(`/admin/tenants/${tenantId}/keys`)).body.items.find(
      (k: Json) => k.label === "in-b-e2e",
    );
    await calls(keyB, 4);
    await calls(internalKeyB, 4);
    expect(upB!.requests.length - before.b).toBe(8);
    expect(upA!.requests.length - before.a).toBe(0);
    expect(throughB?.effective_provider_groups, JSON.stringify(throughB)).toEqual([GROUP_B.name]);
  });

  test("With its group's upstream down, a key gets the outage answer though another group is healthy", async () => {
    upA!.failWith = 500;
    try {
      const res = await callGateway(app!.proxyUrl, keyA, "/v1/chat/completions", chat(ROUTE));
      expect(res.status, res.text).not.toBe(200);
      // The other group's keys still work, so the outage is A's alone: no
      // request crosses from A's key to B's account.
      expect(await modelsStatus(keyB)).toBe(200);
      expect(await modelsStatus(internalKeyB)).toBe(200);
      const other = await callGateway(app!.proxyUrl, keyB, "/v1/chat/completions", chat(ROUTE));
      expect(other.status, other.text).toBe(200);
    } finally {
      upA!.failWith = undefined;
    }
  });

  test("Removing the key's group from its tenant refuses the key at the gateway, and restoring it reopens it", async () => {
    const tenant = await cp!.admin.get(`/admin/tenants/${tenantId}`);
    const stripped = await cp!.admin.patch(
      `/admin/tenants/${tenantId}`,
      { allowed_provider_groups: [GROUP_B.name] },
      tenant.version,
    );
    expect(stripped.status, stripped.text).toBe(200);
    await expect.poll(() => modelsStatus(keyA), { timeout: 30_000 }).toBe(401);
    // The key's own group never changed, and B's key is untouched.
    const admin = await cp!.admin.get(`/admin/keys/${keyAId}`);
    expect(admin.body.group?.name, admin.text).toBe(GROUP_A.name);
    expect(admin.body.effective_disabled, admin.text).toBe(true);
    expect(await modelsStatus(keyB)).toBe(200);

    const restored = await cp!.admin.patch(
      `/admin/tenants/${tenantId}`,
      { allowed_provider_groups: [GROUP_A.name, GROUP_B.name] },
      (await cp!.admin.get(`/admin/tenants/${tenantId}`)).version,
    );
    expect(restored.status, restored.text).toBe(200);
    await expect.poll(() => modelsStatus(keyA), { timeout: 30_000 }).toBe(200);
  });
});
