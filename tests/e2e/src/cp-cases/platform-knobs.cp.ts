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
import {
  callGateway,
  startPlatformUpstream,
  type GatewayAnswer,
  textWithoutPassedThroughIds,
  type PlatformUpstream,
} from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";

// E2E: the two configuration rules a platform deployment keeps tenants apart
// with, end to end: the response cache shared only within a tenant, and the
// provider groups that confine a tenant's keys to the upstream accounts in
// them. Plus the control plane refusing the cache scope that would share
// entries across tenants.

const LEAKS = ["acct-alpha-e2e", "acct-beta-e2e", "grp-alpha-7c1", "grp-beta-3e9"];
const chat = (model: string) => ({ model, messages: [{ role: "user", content: "the same question" }] });

describe("platform configuration rules at the gateway", () => {
  let cp: ControlPlaneStack | undefined;
  let app: ManagedGateway | undefined;
  /** The account behind the cached model. */
  let cached: PlatformUpstream | undefined;
  /** Mock A and mock B: one account each, in groups of their own. */
  let alpha: PlatformUpstream | undefined;
  let beta: PlatformUpstream | undefined;
  const keys: Record<string, string> = {};

  /** An active tenant, as an operator creates one; its id. */
  const tenant = async (name: string, patch: Record<string, unknown>): Promise<string> => {
    const created = await cp!.admin.post("/admin/tenants", { name });
    expect(created.status, created.text).toBe(201);
    const updated = await cp!.admin.patch(`/admin/tenants/${created.body.id}`, patch, created.body.version);
    expect(updated.status, updated.text).toBe(200);
    await fundTenant(cp!, created.body.id, "100.00");
    return created.body.id;
  };
  /** A tenant's key; `group_id` when the tenant is assigned more than one. */
  const mint = async (tenantId: string, label: string, groupId?: string): Promise<string> => {
    const res = await cp!.admin.post(`/admin/tenants/${tenantId}/keys`, {
      label,
      ...(groupId ? { group_id: groupId } : {}),
    });
    expect(res.status, res.text).toBe(201);
    return res.body.plaintext;
  };
  const account = (name: string, upstream: PlatformUpstream, groups: string[]): Promise<Json> =>
    cp!.admin.create("provider_keys", {
      display_name: name,
      provider: "openai",
      api_key: `sk-${name}-upstream`,
      api_base: `${upstream.baseUrl}/v1`,
      groups,
    });
  const direct = (name: string, provider: Json): Promise<Json> =>
    cp!.admin.create("models", {
      display_name: name,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: provider.id,
    });

  beforeAll(async () => {
    cached = await startPlatformUpstream("knob-cache-up");
    // The marker each upstream puts in its ids and its error text names its
    // own account and group, as a reseller's channel ids often do.
    alpha = await startPlatformUpstream("acct-alpha-e2e.grp-alpha-7c1");
    beta = await startPlatformUpstream("acct-beta-e2e.grp-beta-3e9");
    cp = await startControlPlane();
    const gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw);
    await cp.admin.waitRegistered(gw.dpId);

    const cachedModel = await direct("knob-cached-e2e", await account("knob-cache-account", cached, ["shared"]));
    await cp.admin.create("cache_policies", {
      name: "knob-team-cache",
      scope: "team",
      applies_to_model_id: cachedModel.id,
    });
    // The group a tenant is assigned must exist: a key names it, and
    // `grp-alpha-7c1` was only a name in the account's document.
    await cp.admin.createGroup("grp-alpha-7c1", "甲组 7c1");
    await cp.admin.createGroup("grp-beta-3e9", "乙组 3e9");
    const alphaModel = await direct("knob-alpha-e2e", await account("acct-alpha-e2e", alpha, ["grp-alpha-7c1"]));
    const betaModel = await direct("knob-beta-e2e", await account("acct-beta-e2e", beta, ["grp-beta-3e9"]));
    // B first, round robin: a group check that let B through would show.
    const route = await cp.admin.create("models", {
      display_name: "knob-route-e2e",
      routing: { strategy: "round_robin", targets: [{ model_id: betaModel.id }, { model_id: alphaModel.id }] },
    });

    const t1 = await tenant("Knob E2E One", { allowed_model_ids: [cachedModel.id] });
    const t2 = await tenant("Knob E2E Two", { allowed_model_ids: [cachedModel.id] });
    const t3 = await tenant("Knob E2E Groups", {
      allowed_model_ids: [route.id],
      allowed_provider_groups: ["grp-alpha-7c1"],
    });
    // The keys are written last; each authenticating is the readiness gate.
    keys.t1a = await mint(t1, "knob-t1-a");
    keys.t1b = await mint(t1, "knob-t1-b");
    keys.t2 = await mint(t2, "knob-t2");
    // t3 is assigned exactly one group, so its key takes that one.
    keys.t3 = await mint(t3, "knob-t3");
    for (const [name, key] of Object.entries(keys)) {
      const proxy = new ProxyClient(app.proxyUrl, key);
      await pollUntil(`key ${name} to authenticate`, async () =>
        (await proxy.listModels()).status === 200 ? true : undefined,
      );
    }
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await Promise.all([cached?.close(), alpha?.close(), beta?.close()]);
  });

  test("Keys of one tenant share the cache; different tenants do not", async () => {
    const counts: number[] = [];
    for (const key of [keys.t1a, keys.t1b, keys.t2]) {
      const res = await callGateway(app!.proxyUrl, key, "/v1/chat/completions", chat("knob-cached-e2e"));
      expect(res.status, res.text).toBe(200);
      counts.push(cached!.count("/v1/chat/completions"));
    }
    expect(counts).toEqual([1, 1, 2]);
  });

  test("A key's provider groups confine it to upstreams in those groups", async () => {
    // Every answer the key gets here, served or failed, is scanned below.
    const answers: GatewayAnswer[] = [];
    const call = async (): Promise<GatewayAnswer> => {
      const res = await callGateway(app!.proxyUrl, keys.t3, "/v1/chat/completions", chat("knob-route-e2e"));
      answers.push(res);
      return res;
    };
    for (let i = 0; i < 10; i++) {
      const res = await call();
      expect(res.status, res.text).toBe(200);
    }
    expect(alpha!.count("/v1/chat/completions")).toBe(10);
    expect(beta!.count("/v1/chat/completions")).toBe(0);

    alpha!.failWith = 500;
    const failed = await call();
    expect(failed.status, failed.text).toBeGreaterThanOrEqual(400);
    expect(alpha!.count("/v1/chat/completions")).toBeGreaterThan(10);
    expect(beta!.count("/v1/chat/completions")).toBe(0);

    // The served answers carry what the upstream issued, the failed one its
    // error: none names the account or the group behind the routing target,
    // in its body or its headers.
    expect(answers.length).toBe(11);
    for (const answer of answers) {
      const headers = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
      for (const leak of LEAKS) {
        expect(textWithoutPassedThroughIds(answer), `a ${answer.status} body names ${leak}`).not.toContain(leak);
        expect(headers, `a ${answer.status} answer's headers name ${leak}`).not.toContain(leak);
      }
    }
  });

  test("An env-scoped cache policy is stopped at the control plane", async () => {
    const res = await cp!.admin.post("/admin/resources/cache_policies", {
      doc: { name: "knob-env-cache", scope: "env" },
    });
    expect(res.status, res.text).toBe(422);
  });
});
