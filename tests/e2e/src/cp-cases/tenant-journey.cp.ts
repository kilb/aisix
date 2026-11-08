import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  fundTenant,
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type CpPortal,
  type IssuedGateway,
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

// E2E: the whole tenant journey across both planes. An operator sets up an
// upstream account and platform models; a company signs up, is approved and
// granted models; its owner mints a key on the tenant portal; the key calls
// the gateway; and the operator's member and tenant switches reach that key
// within the rollout bound.

/** The channel marker the upstream puts in every id it issues. */
const MARKER = "chan-mk-9f3";
const OWNER = { org: "Journey E2E Co", email: "owner@journey-e2e.test", password: "journey-e2e-pass-7c1" };
/** How long a switch on the control plane may take to reach the gateway. */
const ROLLOUT_BOUND_MS = 15_000;

const chatBody = (model: string, stream = false) => ({
  model,
  stream,
  messages: [{ role: "user", content: "hi" }],
});

describe("a tenant's journey from signup to a working key, and the switches that govern it", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: PlatformUpstream | undefined;
  let gw: IssuedGateway;
  let app: ManagedGateway | undefined;
  let owner: CpPortal;
  let tenantId: string;
  const accounts: Json[] = [];
  const models: Record<string, Json> = {};
  /** The owner's main key, and a second one that only counts tokens. */
  let mainKey: { id: string; plaintext: string };
  let countKey: { id: string; plaintext: string; version: number };
  /** Every answer a tenant key got from the gateway, for the last test's scan. */
  const answers: GatewayAnswer[] = [];

  const call = async (key: string, path: string, body: unknown, headers?: Record<string, string>) => {
    const answer = await callGateway(app!.proxyUrl, key, path, body, headers);
    answers.push(answer);
    return answer;
  };
  const listStatus = async (key: string) => (await call(key, "/v1/models", undefined)).status;
  const waitListStatus = (key: string, status: number, what: string) => {
    let last = 0;
    return pollUntil(what, async () => ((last = await listStatus(key)) === status ? true : undefined), {
      timeoutMs: ROLLOUT_BOUND_MS,
      describe: () => `last status ${last}`,
    });
  };

  beforeAll(async () => {
    upstream = await startPlatformUpstream(MARKER);
    cp = await startControlPlane();
    gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw);
    // The gateway has registered before anything it would be judged by is
    // written (the save-time version gate of a stamped control plane).
    await cp.admin.waitRegistered(gw.dpId);
    // `shared` at 1 CNY per USD: a charge in µ¥ is the card's own µ$, so the
    // amounts these cases were written with hold.
    await cp.admin.ensureSharedRate();

    // The operator's catalog. Every account a tenant reaches is in `shared`.
    accounts.push(
      await cp.admin.create("provider_keys", {
        display_name: "acct-e2e-1",
        provider: "openai",
        api_key: "sk-acct-e2e-1-upstream",
        api_base: `${upstream.baseUrl}/v1`,
        groups: ["shared"],
      }),
      await cp.admin.create("provider_keys", {
        display_name: "acct-e2e-ant",
        provider: "anthropic",
        adapter: "anthropic",
        api_key: "sk-ant-acct-e2e-upstream",
        api_base: upstream.baseUrl,
        groups: ["shared"],
      }),
    );
    const direct = (name: string, upstreamModel: string, account: Json, provider = "openai") =>
      cp!.admin.create("models", {
        display_name: name,
        provider,
        model_name: upstreamModel,
        provider_key_id: account.id,
      });
    models.chat = await direct("chat-e2e", "gpt-4o-mini", accounts[0]);
    models.embed = await direct("embed-e2e", "text-embedding-3-small", accounts[0]);
    // Token counting needs an upstream that counts tokens.
    models.count = await direct("count-e2e", "claude-haiku-4-5", accounts[1], "anthropic");
    // In the catalog, never granted to this tenant.
    models.other = await direct("other-e2e", "gpt-4o", accounts[0]);
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.stop();
    await upstream?.close();
  });

  test("an owner signs up, and an operator approves the company and grants it models", async () => {
    owner = cp!.portal();
    const signup = await owner.signup(OWNER.org, OWNER.email, OWNER.password);
    expect(signup.status, signup.text).toBe(201);
    expect(signup.body.tenant.status).toBe("pending");
    tenantId = signup.body.tenant.id;

    const approved = await cp!.admin.post(`/admin/tenants/${tenantId}/approve`);
    expect(approved.status, approved.text).toBe(200);
    expect(approved.body.status).toBe("active");

    const current = await cp!.admin.get(`/admin/tenants/${tenantId}`);
    const granted = await cp!.admin.patch(
      `/admin/tenants/${tenantId}`,
      { allowed_model_ids: [models.chat.id, models.embed.id, models.count.id] },
      current.version,
    );
    expect(granted.status, granted.text).toBe(200);
    await fundTenant(cp!, tenantId, "100.00");
    await cp!.admin.waitTenantApplied(tenantId);
  });

  test("the owner's key reaches exactly its models on every first-phase endpoint, buffered and streamed", async () => {
    const catalog = await owner.get("/api/models");
    expect(catalog.status, catalog.text).toBe(200);
    const idOf = (name: string) => (catalog.body.items as Json[]).find((m) => m.name === name)?.id;

    // Written last: the main key authenticating is the readiness gate.
    const count = await owner.post("/api/keys", { label: "journey-count", model_ids: [idOf("count-e2e")] });
    expect(count.status, count.text).toBe(201);
    countKey = { id: count.body.key.id, plaintext: count.body.plaintext, version: count.body.key.version };
    const main = await owner.post("/api/keys", {
      label: "journey-main",
      model_ids: [idOf("chat-e2e"), idOf("embed-e2e")],
    });
    expect(main.status, main.text).toBe(201);
    mainKey = { id: main.body.key.id, plaintext: main.body.plaintext };
    await waitListStatus(countKey.plaintext, 200, "the count key to authenticate");
    await waitListStatus(mainKey.plaintext, 200, "the main key to authenticate");

    const listed = await call(mainKey.plaintext, "/v1/models", undefined);
    const ids = (listed.json.data as Json[]).map((m) => m.id).sort();
    expect(ids).toEqual(["chat-e2e", "embed-e2e"]);

    for (const stream of [false, true]) {
      const chat = await call(mainKey.plaintext, "/v1/chat/completions", chatBody("chat-e2e", stream));
      expect(chat.status, `chat stream=${stream}: ${chat.text}`).toBe(200);
      const messages = await call(mainKey.plaintext, "/v1/messages", {
        model: "chat-e2e",
        stream,
        max_tokens: 64,
        messages: [{ role: "user", content: "hi" }],
      });
      expect(messages.status, `messages stream=${stream}: ${messages.text}`).toBe(200);
      const responses = await call(mainKey.plaintext, "/v1/responses", { model: "chat-e2e", stream, input: "hi" });
      expect(responses.status, `responses stream=${stream}: ${responses.text}`).toBe(200);
      if (stream) {
        for (const answer of [chat, messages, responses]) {
          expect(answer.headers.get("content-type") ?? "").toContain("text/event-stream");
        }
      }
    }

    const counted = await call(countKey.plaintext, "/v1/messages/count_tokens", {
      model: "count-e2e",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(counted.status, counted.text).toBe(200);
    expect(counted.json.input_tokens).toBe(7);

    const embedded = await call(mainKey.plaintext, "/v1/embeddings", { model: "embed-e2e", input: "hi" });
    expect(embedded.status, embedded.text).toBe(200);
    expect(embedded.json.data[0].embedding).toEqual([0.25, 0.5]);

    // Every one of those calls reached the upstream.
    expect(upstream!.requests.length).toBeGreaterThanOrEqual(8);
  });

  test("routes outside the first phase are refused to the tenant key before any upstream call", async () => {
    const outside: Array<[string, Record<string, unknown>]> = [
      ["/v1/images/generations", { model: "chat-e2e", prompt: "a cat" }],
      ["/v1/completions", { model: "chat-e2e", prompt: "hi" }],
    ];
    for (const [path, body] of outside) {
      const before = upstream!.count(path);
      const refused = await call(mainKey.plaintext, path, body);
      expect(refused.status, `${path}: ${refused.text}`).toBe(403);
      expect(refused.json?.error?.type, `${path}: ${refused.text}`).toBe("permission_denied");
      expect(upstream!.count(path), path).toBe(before);
    }
  });

  test("a model the tenant was not granted answers exactly like one that does not exist", async () => {
    const ungranted = await call(mainKey.plaintext, "/v1/chat/completions", chatBody("other-e2e"));
    const missing = await call(mainKey.plaintext, "/v1/chat/completions", chatBody("missing-e2e-4b1"));
    const both = `ungranted: ${ungranted.text}\nmissing: ${missing.text}`;
    // A model that does not exist is 404 in the OpenAI contract: pinned, so
    // that two answers alike for another reason (both 401, both 5xx) fail.
    expect(missing.status, both).toBe(404);
    expect(ungranted.status, both).toBe(missing.status);
    expect(ungranted.json?.error, both).toBeDefined();
    expect(ungranted.json.error.code, both).toBe(missing.json?.error?.code);
    expect(ungranted.json.error.type, both).toBe(missing.json?.error?.type);
    // The same answer but for the name asked about.
    const message = (answer: GatewayAnswer, name: string) =>
      String(answer.json?.error?.message).replaceAll(name, "<model>");
    expect(message(ungranted, "other-e2e"), both).toBe(message(missing, "missing-e2e-4b1"));
  });

  test("a model the operator renames keeps answering the key under its new name", async () => {
    const current = await cp!.admin.get(`/admin/resources/models/${models.chat.id}`);
    const renamed = await cp!.admin.put(
      `/admin/resources/models/${models.chat.id}`,
      { doc: { ...current.body.doc, display_name: "chat-e2e-renamed" } },
      current.version!,
    );
    expect(renamed.status, renamed.text).toBe(200);
    await cp!.admin.waitApplied("models", models.chat.id);

    const res = await call(mainKey.plaintext, "/v1/chat/completions", chatBody("chat-e2e-renamed"));
    expect(res.status, res.text).toBe(200);
  });

  test("disabling the owner's member account disables the key, and re-enabling it restores the key", async () => {
    // A company always keeps one owner who can sign in, so the owner first
    // brings in a co-owner.
    const invite = await owner.post("/api/members/invites", { email: "co-owner@journey-e2e.test", role: "owner" });
    expect(invite.status, invite.text).toBe(201);
    const accepted = await cp!.portal().post("/api/invites/accept", {
      token: invite.body.invite_token,
      password: "journey-e2e-co-owner-3e9",
    });
    expect(accepted.status, accepted.text).toBe(201);

    const members = await cp!.admin.get(`/admin/tenants/${tenantId}/members`);
    const member = (members.body.items as Json[]).find((m) => m.email === OWNER.email);
    expect(member, members.text).toBeDefined();

    const disabled = await cp!.admin.patch(`/admin/members/${member.id}`, { disabled: true });
    expect(disabled.status, disabled.text).toBe(200);
    await waitListStatus(mainKey.plaintext, 401, "the disabled member's key to be refused");

    const enabled = await cp!.admin.patch(`/admin/members/${member.id}`, { disabled: false });
    expect(enabled.status, enabled.text).toBe(200);
    await waitListStatus(mainKey.plaintext, 200, "the re-enabled member's key to be served");
  });

  test("suspending the tenant disables its keys; reactivating restores them, except one its owner disabled", async () => {
    // The owner signs in again (disabling the member may have ended the
    // session) and switches the count key off.
    const login = await owner.login(OWNER.email, OWNER.password);
    expect(login.status, login.text).toBe(200);
    const current = await owner.get(`/api/keys/${countKey.id}`);
    expect(current.status, current.text).toBe(200);
    const off = await owner.patch(`/api/keys/${countKey.id}`, { disabled: true }, current.version);
    expect(off.status, off.text).toBe(200);
    await waitListStatus(countKey.plaintext, 401, "the key its owner disabled to be refused");

    const suspended = await cp!.admin.post(`/admin/tenants/${tenantId}/suspend`);
    expect(suspended.status, suspended.text).toBe(200);
    await waitListStatus(mainKey.plaintext, 401, "the suspended tenant's key to be refused");

    const reactivated = await cp!.admin.post(`/admin/tenants/${tenantId}/reactivate`);
    expect(reactivated.status, reactivated.text).toBe(200);
    await waitListStatus(mainKey.plaintext, 200, "the reactivated tenant's key to be served");
    // Every document the reactivation rewrote has reached the gateway.
    await cp!.admin.waitTenantApplied(tenantId);
    expect(await listStatus(countKey.plaintext)).toBe(401);
  });

  test(
    "the tenant's requests-per-minute limit holds at the gateway",
    async () => {
      const current = await cp!.admin.get(`/admin/tenants/${tenantId}`);
      const limited = await cp!.admin.patch(
        `/admin/tenants/${tenantId}`,
        { limits: { ...current.body.limits, rpm: 2 } },
        current.version,
      );
      expect(limited.status, limited.text).toBe(200);
      await cp!.admin.waitTenantApplied(tenantId);

      /**
       * Sends chats until one is refused with 429 (within 70 s) and counts
       * the ones that succeeded in the 60 seconds ending at that refusal.
       */
      const measure = async () => {
        const sent: Array<{ at: number; status: number }> = [];
        const deadline = Date.now() + 70_000;
        for (;;) {
          const res = await call(mainKey.plaintext, "/v1/chat/completions", chatBody("chat-e2e-renamed"));
          sent.push({ at: Date.now(), status: res.status });
          if (res.status === 429) break;
          expect(res.status, res.text).toBe(200);
          if (Date.now() > deadline) throw new Error(`no 429 within 70 s: ${JSON.stringify(sent)}`);
          await new Promise((r) => setTimeout(r, 200));
        }
        const refusedAt = sent[sent.length - 1].at;
        return sent.filter((s) => s.status === 200 && s.at >= refusedAt - 60_000).length;
      };

      let succeeded = await measure();
      if (succeeded > 2) {
        // A window boundary fell between the requests: measure once more.
        succeeded = await measure();
      }
      expect(succeeded).toBeLessThanOrEqual(2);
    },
    170_000,
  );

  test("no answer a tenant key got names the upstream channel, account or model", () => {
    // At least these 15: the model list, chat, messages and responses
    // buffered and streamed (6), count_tokens, embeddings, the two refused
    // routes, the ungranted and the missing model, the chat after the
    // rename, and the rpm limit's 429. The readiness and switch polls come
    // on top.
    expect(answers.length).toBeGreaterThanOrEqual(15);
    expect(answers.filter((a) => a.status === 403).length).toBeGreaterThanOrEqual(2);
    expect(answers.some((a) => a.status === 429)).toBe(true);
    // A Chat Completions response id is the upstream's own, unchanged: the
    // mock upstream puts the marker in it, so the sweep below skips ids on
    // the two surfaces where they pass through.
    const chatAnswer = answers.find((a) => a.path === "/v1/chat/completions" && a.status === 200);
    expect(chatAnswer?.json?.id, chatAnswer?.text).toBe(`chatcmpl-${MARKER}`);
    const leaks = [
      MARKER,
      "acct-e2e-1",
      "acct-e2e-ant",
      accounts[0].id,
      accounts[1].id,
      // The upstream model ids behind chat-e2e, other-e2e, embed-e2e and count-e2e.
      "gpt-4o-mini",
      "gpt-4o",
      "text-embedding-3-small",
      "claude-haiku-4-5",
    ];
    for (const answer of answers) {
      const headers = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
      for (const leak of leaks) {
        expect(textWithoutPassedThroughIds(answer), `a ${answer.status} body names ${leak}`).not.toContain(leak);
        expect(headers, `a ${answer.status} answer's headers name ${leak}`).not.toContain(leak);
      }
    }
  });
});
