import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  pollUntil,
  sleep,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type IssuedGateway,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { ProxyClient } from "../harness/proxy.js";
import { startOpenAiUpstream, type OpenAiUpstream } from "../harness/upstream-openai.js";

// E2E: what an operator saves on the control plane reaches a gateway that
// runs in managed mode from the credential bundle alone, over the real
// mTLS link and etcd front, and the control plane reports how far each
// change got.

const chat = (model: string) => ({ model, messages: [{ role: "user", content: "hi" }] });

/** An upstream account (in the default `shared` group) and one model on it. */
async function seedModel(cp: ControlPlaneStack, upstream: OpenAiUpstream, name: string): Promise<Json> {
  const account = await cp.admin.create("provider_keys", {
    display_name: `${name}-account`,
    provider: "openai",
    api_key: "sk-cfg-upstream-e2e",
    api_base: `${upstream.baseUrl}/v1`,
    groups: ["shared"],
  });
  return cp.admin.create("models", {
    display_name: name,
    provider: "openai",
    model_name: "gpt-4o-mini",
    provider_key_id: account.id,
  });
}

/** An operator's key on the platform's own tenant, for every model; its plaintext. */
async function mintInternalKey(cp: ControlPlaneStack): Promise<string> {
  const tenants = await cp.admin.get("/admin/tenants");
  const internal = (tenants.body.items as Json[]).find((t) => t.is_internal);
  expect(internal, tenants.text).toBeDefined();
  const res = await cp.admin.post(`/admin/tenants/${internal.id}/keys`, {
    label: "cfg-caller",
    allowed_models: ["*"],
  });
  expect(res.status, res.text).toBe(201);
  return res.body.plaintext;
}

/** The readiness gate: the caller key, written last, authenticates. */
async function gate(app: ManagedGateway, key: string): Promise<void> {
  const proxy = new ProxyClient(app.proxyUrl, key);
  let last = 0;
  await pollUntil(
    "the caller key to authenticate",
    async () => ((last = (await proxy.listModels()).status) === 200 ? true : undefined),
    { describe: () => `last status ${last}` },
  );
}

describe("a gateway booted from the bundle alone", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: OpenAiUpstream | undefined;
  let gw: IssuedGateway;
  let app: ManagedGateway | undefined;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    cp = await startControlPlane({ etcdPrefix: "/aisix-alt-e2e" });
    gw = await cp.issueGateway();
    // Nothing but the bundle and this machine's local paths and ports.
    app = await spawnManagedGateway(gw);
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await upstream?.close();
  });

  test("A gateway started only from the issued bundle, on a non-default prefix, comes up healthy and serves", async () => {
    const instance = await pollUntil(
      "the gateway's instance to be online with an applied revision",
      async () => {
        const i = await cp!.admin.instance(gw.dpId);
        return i?.online === true && i.applied_revision > 0 ? i : undefined;
      },
    );
    expect(instance.online).toBe(true);

    await seedModel(cp!, upstream!, "alt-chat-e2e");
    const key = await mintInternalKey(cp!);
    await gate(app!, key);

    const res = await new ProxyClient(app!.proxyUrl, key).chat(chat("alt-chat-e2e"));
    expect(res.status, JSON.stringify(res.body)).toBe(200);
  });
});

describe("configuration the control plane saves reaches the gateway", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: OpenAiUpstream | undefined;
  let gw: IssuedGateway;
  let app: ManagedGateway | undefined;
  let model: Json;
  let key: string;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    cp = await startControlPlane();
    gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw);
    await cp.admin.waitRegistered(gw.dpId);
    model = await seedModel(cp, upstream, "cfg-chat-e2e");
    key = await mintInternalKey(cp);
    await gate(app, key);
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await upstream?.close();
  });

  test("A model an operator saves reaches the gateway and is reported applied", async () => {
    const res = await new ProxyClient(app!.proxyUrl, key).chat(chat("cfg-chat-e2e"));
    expect(res.status, JSON.stringify(res.body)).toBe(200);
    expect(upstream!.receivedRequests.length).toBe(1);

    const view = await cp!.admin.waitApplied("models", model.id);
    expect(view.rollout.state).toBe("applied");
    expect(view.rollout.applied).toBe(1);
    expect(view.rollout.online).toBe(1);
  });

  test("A guardrail the gateway cannot build is reported rejected with the gateway's reason, and the rest keeps serving", async () => {
    // Attached to a model nobody calls, so only its build is in question.
    const quiet = await seedModel(cp!, upstream!, "cfg-quiet-e2e");
    const guardrail = await cp!.admin.create("guardrails", {
      name: "cfg-unbuildable-e2e",
      kind: "keyword",
      hook_point: "input",
      patterns: [{ kind: "regex", value: "(" }],
    });
    await cp!.admin.create("guardrail_attachments", {
      guardrail_id: guardrail.id,
      scope_type: "model",
      scope_id: quiet.id,
      priority: 100,
    });

    let last: Json;
    const view = await pollUntil(
      "the guardrail to be reported rejected",
      async () => {
        last = (await cp!.admin.get(`/admin/resources/guardrails/${guardrail.id}`)).body;
        return last.rollout?.state === "rejected" ? last : undefined;
      },
      { describe: () => JSON.stringify(last?.rollout) },
    );
    expect(view.rollout.rejections.length).toBeGreaterThan(0);
    expect(typeof view.rollout.rejections[0].error).toBe("string");
    expect(view.rollout.rejections[0].error.length).toBeGreaterThan(0);

    const summary = await cp!.admin.get("/admin/rollout");
    const named = (summary.body.rejected as Json[]).find((r) => r.id === guardrail.id);
    expect(named, summary.text).toBeDefined();
    expect(named.name).toBe("cfg-unbuildable-e2e");

    const res = await new ProxyClient(app!.proxyUrl, key).chat(chat("cfg-chat-e2e"));
    expect(res.status, JSON.stringify(res.body)).toBe(200);
  });

  test("One config change, and the reported revision catches up", async () => {
    const current = await cp!.admin.get(`/admin/resources/models/${model.id}`);
    expect(current.status, current.text).toBe(200);
    const changed = await cp!.admin.put(
      `/admin/resources/models/${model.id}`,
      { doc: { ...current.body.doc, timeout: 45_000 } },
      current.version!,
    );
    expect(changed.status, changed.text).toBe(200);
    expect(changed.body.rollout.state).not.toBe("applied");

    const view = await cp!.admin.waitApplied("models", model.id);
    expect(view.doc.timeout).toBe(45_000);
    expect(view.rollout.state).toBe("applied");
  });

  test("The bundle never sets an etcd user, and a gateway that sets one fails loudly", async () => {
    expect(Object.keys(gw.bundleEnv)).not.toContain("AISIX_ETCD__USER");
    expect(Object.keys(gw.bundleEnv)).not.toContain("AISIX_ETCD__PASSWORD_ENV");

    const other = await cp!.issueGateway("cfg-etcd-user");
    const bad = await spawnManagedGateway(other, {
      awaitReady: false,
      env: {
        AISIX_ETCD__USER: "x",
        AISIX_ETCD__PASSWORD_ENV: "E2E_ETCD_PW",
        E2E_ETCD_PW: "cfg-etcd-password-e2e",
      },
    });
    let answered = false;
    let probing = true;
    const probe = (async () => {
      while (probing) {
        try {
          if ((await fetch(`${bad.proxyUrl}/livez`)).status === 200) answered = true;
        } catch {
          // Nothing listening: what a gateway that never booted looks like.
        }
        await sleep(100);
      }
    })();
    try {
      const code = await bad.waitForExit(20_000);
      expect(code, bad.output()).not.toBe(0);
      expect(code, bad.output()).not.toBeNull();
    } finally {
      probing = false;
      await probe;
      await bad.exit();
    }
    expect(answered).toBe(false);
  });
});

describe("a revoked gateway credential", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: OpenAiUpstream | undefined;
  let gw: IssuedGateway;
  let app: ManagedGateway | undefined;
  let key: string;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    cp = await startControlPlane();
    gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw);
    await cp.admin.waitRegistered(gw.dpId);
    await seedModel(cp, upstream, "rev-chat-e2e");
    key = await mintInternalKey(cp);
    await gate(app, key);
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await upstream?.close();
  });

  test("A revoked credential: the instance goes offline and config written afterwards is not applied by it", async () => {
    await pollUntil("the gateway's instance to be online", async () =>
      (await cp!.admin.instance(gw.dpId))?.online === true ? true : undefined,
    );
    const revoked = await cp!.admin.post(`/admin/gateways/credentials/${gw.dpId}/revoke`);
    expect(revoked.status, revoked.text).toBe(200);

    await pollUntil(
      "the revoked gateway's instance to go offline",
      async () => ((await cp!.admin.instance(gw.dpId))?.online === false ? true : undefined),
      { timeoutMs: 15_000 },
    );

    const late = await seedModel(cp!, upstream!, "rev-late-e2e");
    let state: unknown;
    await pollUntil(
      "the model written after the revocation to be reported as reaching no gateway",
      async () => {
        state = (await cp!.admin.get(`/admin/resources/models/${late.id}`)).body.rollout?.state;
        return state === "no_gateways" ? true : undefined;
      },
      { describe: () => String(state) },
    );

    // The model is in kine now. A gateway still watching would have it
    // within milliseconds; give it a generous moment, then look.
    await sleep(2_000);
    const listed = await new ProxyClient(app!.proxyUrl, key).listModels();
    expect(listed.status).toBe(200);
    const ids = ((listed.body as Json).data as Json[]).map((m) => m.id);
    expect(ids).toContain("rev-chat-e2e");
    expect(ids).not.toContain("rev-late-e2e");
  });
});
