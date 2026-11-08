import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { startPlatformUpstream, type PlatformUpstream } from "../harness/cp-upstream.js";
import {
  fundTenant,
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { ProxyClient } from "../harness/proxy.js";
import { callGateway } from "../harness/cp-upstream.js";

// E2E: a public model served by several upstream accounts, as the
// channel-style configuration authors it. The operator ticks the model on
// two accounts in one group and creates it on one outside it; the control
// plane writes the direct and routing documents; and a tenant key's
// requests reach only the accounts inside its group.
//
// The requests are the observable contract: which account's fixture was hit,
// how often, and what a caller is told when no account is left. An assertion
// here is about distribution and refusal, never about a count the product
// picks — the weighted split is asserted as a band, not an exact figure.
//
// A charge's ¥ figure and the margin filter by account belong to the CNY
// charge table and the margin route, which are not on this branch; the charge
// this case can assert is that the public model's id — not a serving
// account's — is what the request is billed under.

const ACCOUNTS = { a: "acct-chan-a-7c1", b: "acct-chan-b-7c1", c: "acct-chan-c-7c1" };
const GROUP = "grp-gp-7c1";
const OTHER_GROUP = "grp-other-7c1";
const PUBLIC = "pub-x-8c1";
const ALONE = "pub-c-8c1";

const chat = (model: string) => ({ model, messages: [{ role: "user", content: "hi" }] });

describe("a public model served by several accounts routes within the caller's group", () => {
  let cp: ControlPlaneStack | undefined;
  let gateway: ManagedGateway | undefined;
  let a: PlatformUpstream | undefined;
  let b: PlatformUpstream | undefined;
  let c: PlatformUpstream | undefined;
  let publicModel: Json;
  let aloneModel: Json;
  let key: string;

  /** An account in `group`, behind its own fixture. */
  const account = (name: string, upstream: PlatformUpstream, group: string): Promise<Json> =>
    cp!.admin.create("provider_keys", {
      display_name: name,
      provider: "openai",
      api_key: `sk-${name}-upstream`,
      api_base: `${upstream.baseUrl}/v1`,
      groups: [group],
    });

  /** `PUT` the channel model for `accountId`/`publicId`; the answer. */
  const serve = (
    accountId: string,
    publicId: string,
    body: Record<string, unknown>,
  ): Promise<{ status: number; text: string; body: Json }> =>
    cp!.admin
      .put(`/admin/provider-keys/${accountId}/channel-models/${publicId}`, body)
      .then((res) => ({ status: res.status, text: res.text, body: res.body }));

  /** The fixture hit counts at the chat endpoint. */
  const hits = (): [number, number, number] => [
    a!.count("/v1/chat/completions"),
    b!.count("/v1/chat/completions"),
    c!.count("/v1/chat/completions"),
  ];

  /** `n` requests to `model` with the caller key; each answer. */
  const request = async (model: string, n: number) => {
    const answers = [];
    for (let i = 0; i < n; i++) {
      answers.push(await callGateway(gateway!.proxyUrl, key, "/v1/chat/completions", chat(model)));
    }
    return answers;
  };

  beforeAll(async () => {
    a = await startPlatformUpstream(ACCOUNTS.a);
    b = await startPlatformUpstream(ACCOUNTS.b);
    c = await startPlatformUpstream(ACCOUNTS.c);
    cp = await startControlPlane();
    const gw = await cp.issueGateway();
    gateway = await spawnManagedGateway(gw);
    await cp.admin.waitRegistered(gw.dpId);

    const accountA = await account(ACCOUNTS.a, a, GROUP);
    const accountB = await account(ACCOUNTS.b, b, GROUP);
    const accountC = await account(ACCOUNTS.c, c, OTHER_GROUP);

    const created = await cp.admin.post("/admin/public-models", { name: PUBLIC });
    expect(created.status, created.text).toBe(201);
    publicModel = created.body;
    const alone = await cp.admin.post("/admin/public-models", { name: ALONE });
    expect(alone.status, alone.text).toBe(201);
    aloneModel = alone.body;

    // A's tier is above B's, so A alone serves until its priority drops:
    // neither fixture can be hit by the other's share.
    let res = await serve(accountA.id, publicModel.id, { priority: 1, weight: 1 });
    expect(res.status, res.text).toBe(200);
    res = await serve(accountB.id, publicModel.id, { priority: 0, weight: 1 });
    expect(res.status, res.text).toBe(200);
    res = await serve(accountC.id, aloneModel.id, { priority: 0, weight: 1 });
    expect(res.status, res.text).toBe(200);

    // A tenant confined to the public model's group, and one to the third
    // account's, so neither can reach the other's accounts.
    const tenant = await cp.admin.post("/admin/tenants", { name: "Channel E2E" });
    expect(tenant.status, tenant.text).toBe(201);
    const granted = await cp.admin.patch(
      `/admin/tenants/${tenant.body.id}`,
      {
        allowed_model_ids: [publicModel.id, aloneModel.id],
        allowed_provider_groups: [GROUP],
      },
      tenant.body.version,
    );
    expect(granted.status, granted.text).toBe(200);
    // Postpaid with room to spend: this case is about routing, not billing.
    await fundTenant(cp, tenant.body.id, "100.00");
    const minted = await cp.admin.post(`/admin/tenants/${tenant.body.id}/keys`, {
      label: "channel-caller",
    });
    expect(minted.status, minted.text).toBe(201);
    key = minted.body.plaintext;

    // The readiness gate: the caller key authenticates, which implies every
    // document written before it is in the gateway's snapshot.
    const proxy = new ProxyClient(gateway.proxyUrl, key);
    await pollUntil("the caller key to authenticate", async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
  });

  afterAll(async () => {
    await gateway?.exit();
    await cp?.stop();
    await Promise.all([a?.close(), b?.close(), c?.close()]);
  });

  test("the public model is listed, and the accounts behind it are not", async () => {
    const proxy = new ProxyClient(gateway!.proxyUrl, key);
    const listed = await proxy.listModels();
    expect(listed.status, listed.requestId).toBe(200);
    const ids: string[] = ((listed.body as Json).data as Json[]).map((m) => m.id as string);
    expect(ids).toContain(PUBLIC);
    // A tenant never sees the generated direct documents.
    expect(ids.filter((id) => id.includes("#")), ids.join(",")).toEqual([]);
    for (const account of Object.values(ACCOUNTS)) expect(ids).not.toContain(account);
  });

  test("only the accounts inside the caller's group serve it", async () => {
    const before = hits();
    const answers = await request(PUBLIC, 20);
    for (const answer of answers) expect(answer.status, answer.text).toBe(200);
    const after = hits();
    // A's tier is above B's: every request reaches A, and B waits.
    expect(after[0] - before[0], "A serves the higher tier").toBe(20);
    expect(after[1] - before[1], "B is the lower tier").toBe(0);
    expect(after[2] - before[2], "C is outside the group").toBe(0);
  });

  test("the other group's account serves its own model, never this one", async () => {
    const before = hits();
    const answers = await request(ALONE, 5);
    // This key's group excludes C's account, so the model is an outage.
    for (const answer of answers) {
      expect(answer.status, answer.text).toBe(503);
      expect(answer.json?.error?.code, answer.text).toBe("upstream_unavailable");
    }
    expect(hits(), "no account outside the group is dialled").toEqual(before);
  });

  test("inside one tier the weight is the share, so both accounts serve", async () => {
    const detail = await cp!.admin.get(`/admin/public-models/${publicModel.id}`);
    const served = detail.body.served_by as Json[];
    const byName = (name: string) => served.find((s) => s.display_name === name)!.provider_key_id as string;
    // One tier, A at three times B's weight. `round_robin` honours the
    // weight; `failover` would accept it and serve A alone, which is exactly
    // what this asserts against.
    let res = await serve(byName(ACCOUNTS.a), publicModel.id, { priority: 0, weight: 3 });
    expect(res.status, res.text).toBe(200);
    res = await serve(byName(ACCOUNTS.b), publicModel.id, { priority: 0, weight: 1 });
    expect(res.status, res.text).toBe(200);

    // The routing document is the gate: once it is applied, the tier is what
    // the gateway dispatches from, and the split below is an assertion rather
    // than a wait.
    await cp!.admin.waitApplied("models", publicModel.id);

    const before = hits();
    const answers = await request(PUBLIC, 40);
    for (const answer of answers) expect(answer.status, answer.text).toBe(200);
    const after = hits();
    const servedA = after[0] - before[0];
    const servedB = after[1] - before[1];
    expect(servedA + servedB, "one of the two served every request").toBe(40);
    expect(servedA, "both accounts are hit").toBeGreaterThan(0);
    expect(servedB, "both accounts are hit").toBeGreaterThan(0);
    // A wide band around 3:1, never an exact count.
    expect(servedA, `A ${servedA} vs B ${servedB}`).toBeGreaterThanOrEqual(2 * servedB);
    expect(after[2] - before[2], "C is outside the group").toBe(0);
  });

  test("lowering one account below the other sends every request to the other", async () => {
    const accounts = await cp!.admin.get(`/admin/public-models/${publicModel.id}`);
    expect(accounts.status, accounts.text).toBe(200);
    const accountA = (accounts.body.served_by as Json[]).find((s) => s.display_name === ACCOUNTS.a);
    expect(accountA, accounts.text).toBeDefined();
    const res = await serve(accountA!.provider_key_id as string, publicModel.id, { priority: -1 });
    expect(res.status, res.text).toBe(200);

    await cp!.admin.waitApplied("models", publicModel.id);
    const before = hits();
    const answers = await request(PUBLIC, 20);
    for (const answer of answers) expect(answer.status, answer.text).toBe(200);
    const after = hits();
    expect(after[0] - before[0], "A is now the lower tier").toBe(0);
    expect(after[1] - before[1], "B serves the higher tier").toBe(20);
    expect(after[2] - before[2], "C is still outside the group").toBe(0);
  });

  test("disabling every account leaves it listed and answers the outage, not a 404", async () => {
    const detail = await cp!.admin.get(`/admin/public-models/${publicModel.id}`);
    const served = detail.body.served_by as Json[];
    for (const row of served) {
      const res = await serve(row.provider_key_id as string, publicModel.id, { enabled: false });
      expect(res.status, res.text).toBe(200);
    }

    await cp!.admin.waitApplied("models", publicModel.id);

    // Still listed: the public model itself was not withdrawn.
    const proxy = new ProxyClient(gateway!.proxyUrl, key);
    const listed = await proxy.listModels();
    expect(listed.status, listed.requestId).toBe(200);
    const ids = ((listed.body as Json).data as Json[]).map((m) => m.id as string);
    expect(ids, ids.join(",")).toContain(PUBLIC);

    const before = hits();
    const answer = await callGateway(
      gateway!.proxyUrl,
      key,
      "/v1/chat/completions",
      chat(PUBLIC),
    );
    expect(answer.status, answer.text).toBe(503);
    expect(answer.json?.error?.code, answer.text).toBe("upstream_unavailable");
    expect(hits(), "no disabled account is dialled").toEqual(before);
  });
});
