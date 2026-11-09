import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { fundTenant, pollUntil, type CpPortal, type Json } from "../harness/control-plane.js";
import { callGateway, type GatewayAnswer } from "../harness/cp-upstream.js";
import { ProxyClient } from "../harness/proxy.js";
import { startSwallowRelay, type SwallowRelay } from "../harness/swallow-relay.js";

// E2E: a caller reads its own key's quota at every level with the key
// itself, `GET /v1/key` on the gateway, which asks the control plane for it.
// The gateway's REST link to the control plane (`cp_base_url`) runs through a
// relay that can hold every connection; its etcd link goes straight to the
// control plane, so configuration keeps flowing while the relay holds the
// lookups back. Only the upstream model is a stand-in. Each chat costs
// $0.01.

/** How long the gateway serves a lookup from its cache. */
const QUOTA_TTL_MS = 3_000;
/** How long the gateway waits for the control plane on a lookup. */
const ACCOUNT_TIMEOUT_MS = 2_000;
/**
 * A chat during the outage must answer well inside the lookup's deadline: a
 * model request that waited on the control plane would take the whole
 * deadline. The margin is for a loaded machine.
 */
const CHAT_DURING_OUTAGE_MS = 1_500;
const PASSWORD = "key-lookup-e2e-pass-4d2";

describe("a caller looks up its own quota with its API key", () => {
  let world: BillingWorld | undefined;
  let relay: SwallowRelay | undefined;
  let tenantA: BillingTenant;
  let tenantB: BillingTenant;
  /** The owner's key. */
  let ko: string;
  /** Member M's key. */
  let km: string;

  const lookup = (key: string): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, key, "/v1/key", undefined);
  const timedLookup = async (key: string): Promise<{ answer: GatewayAnswer; tookMs: number }> => {
    const started = Date.now();
    const answer = await lookup(key);
    return { answer, tookMs: Date.now() - started };
  };
  const chat = (key: string): Promise<GatewayAnswer> =>
    callGateway(world!.gateway.proxyUrl, key, "/v1/chat/completions", {
      model: CHAT_MODEL,
      messages: [{ role: "user", content: "spend a little" }],
    });
  const timedChat = async (key: string): Promise<{ answer: GatewayAnswer; tookMs: number }> => {
    const started = Date.now();
    const answer = await chat(key);
    return { answer, tookMs: Date.now() - started };
  };
  const served = async (key: string): Promise<void> => {
    const answer = await chat(key);
    expect(answer.status, answer.text).toBe(200);
  };

  async function seat(tenantId: string, email: string, role: string): Promise<{ portal: CpPortal; id: string }> {
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

  async function mint(tenantId: string, label: string, memberId: string): Promise<{ plaintext: string; id: string }> {
    const minted = await world!.cp.admin.post(`/admin/tenants/${tenantId}/keys`, { label, member_id: memberId });
    expect(minted.status, minted.text).toBe(201);
    const proxy = new ProxyClient(world!.gateway.proxyUrl, minted.body.plaintext);
    await pollUntil(`${label} to authenticate`, async () =>
      (await proxy.listModels()).status === 200 ? true : undefined,
    );
    return { plaintext: minted.body.plaintext, id: minted.body.key.id };
  }

  beforeAll(async () => {
    world = await billingWorld({
      gateway: async (cp) => {
        relay = await startSwallowRelay(new URL(cp.dpUrl).host);
        return { managed: { cp_base_url: `https://127.0.0.1:${relay.port}` } };
      },
    });
    const w = world;

    // Tenant A: ¥1.00 to spend, an operator's organization quota of ¥0.50,
    // an owner's quota of ¥0.20 on member M, and M's own cumulative cap of
    // ¥0.05 on its key KM. The owner holds KO.
    tenantA = await w.tenant("Key Lookup A", { setup: (id) => fundTenant(w.cp, id, "1.00") });
    const owner = await seat(tenantA.id, "owner@key-lookup-e2e.test", "owner");
    const member = await seat(tenantA.id, "member@key-lookup-e2e.test", "member");
    const team = await w.cp.admin.post(`/admin/tenants/${tenantA.id}/budgets`, { limit_cny: "0.50" });
    expect(team.status, team.text).toBe(201);
    const memberQuota = await owner.portal.post("/api/budgets", {
      scope: "member",
      scope_ref: member.id,
      limit_cny: "0.20",
    });
    expect(memberQuota.status, memberQuota.text).toBe(201);
    const minted = await mint(tenantA.id, "km", member.id);
    km = minted.plaintext;
    ko = (await mint(tenantA.id, "ko", owner.id)).plaintext;
    const cap = await member.portal.post("/api/budgets", {
      scope: "api_key",
      scope_ref: minted.id,
      kind: "cumulative",
      limit_cny: "0.05",
    });
    expect(cap.status, cap.text).toBe(201);

    // Tenant B: ¥2.00 to spend and an organization quota of ¥0.40; its key
    // belongs to no member.
    tenantB = await w.tenant("Key Lookup B", {
      setup: async (id) => {
        await fundTenant(w.cp, id, "2.00");
        const quota = await w.cp.admin.post(`/admin/tenants/${id}/budgets`, { limit_cny: "0.40" });
        expect(quota.status, quota.text).toBe(201);
      },
    });
  });

  afterAll(async () => {
    await world?.close();
    await relay?.close();
  });

  test("A restricted customer key reads its own quota in exact yuan", async () => {
    // The customer's keys are confined to the model endpoints.
    const images = await callGateway(world!.gateway.proxyUrl, km, "/v1/images/generations", {
      model: CHAT_MODEL,
      prompt: "a lighthouse",
    });
    expect(images.status, images.text).toBe(403);

    for (let i = 0; i < 3; i++) await served(km);

    // ¥0.03 spent of the ¥1.00: ¥0.97 available; the organization quota has
    // ¥0.47 left, the smallest amount the owner's key can still spend.
    let last = "no lookup";
    const owner = await pollUntil(
      "the owner's lookup to show the three charges",
      async () => {
        const answer = await lookup(ko);
        last = `${answer.status} ${answer.text}`;
        return answer.status === 200 && answer.json?.data?.balance?.available_usd === "0.970000"
          ? answer.json.data
          : undefined;
      },
      { timeoutMs: 5_000, intervalMs: 250, describe: () => last },
    );
    expect(owner.effective_remaining_usd, last).toBe("0.470000");
    expect(owner.binding_level, last).toBe("organization");
    const team = (owner.budgets as Json[]).filter((b) => b.scope === "team");
    expect(team, last).toHaveLength(1);
    expect(team[0].limit_usd, last).toBe("0.500000");
    expect(team[0].remaining_usd, last).toBe("0.470000");

    // KM's own cap has ¥0.02 left, less than M's ¥0.17 and the organization's.
    const mine = await lookup(km);
    expect(mine.status, mine.text).toBe(200);
    expect(mine.json.data.effective_remaining_usd, mine.text).toBe("0.020000");
    expect(mine.json.data.binding_level, mine.text).toBe("key");
  });

  test("A member's key sees no organization balance", async () => {
    const mine = await lookup(km);
    expect(mine.status, mine.text).toBe(200);
    expect(mine.json.data, mine.text).not.toHaveProperty("balance");
    const budgets = mine.json.data.budgets as Json[];
    expect(budgets.map((b) => b.scope).sort(), mine.text).toEqual(["api_key", "member"]);
    const memberQuota = budgets.find((b) => b.scope === "member");
    expect(memberQuota.limit_usd, mine.text).toBe("0.200000");
    expect(memberQuota.remaining_usd, mine.text).toBe("0.170000");
    const keyCap = budgets.find((b) => b.scope === "api_key");
    expect(keyCap.kind, mine.text).toBe("cumulative");
    expect(keyCap.limit_usd, mine.text).toBe("0.050000");
    expect(keyCap.remaining_usd, mine.text).toBe("0.020000");
  });

  test("Figures move after spend", async () => {
    await served(km);
    let last = "no lookup";
    await pollUntil(
      "KM's lookup to show the fourth charge",
      async () => {
        const answer = await lookup(km);
        last = `${answer.status} ${answer.text}`;
        return answer.json?.data?.effective_remaining_usd === "0.010000" ? true : undefined;
      },
      { timeoutMs: QUOTA_TTL_MS + 2_000, intervalMs: 250, describe: () => last },
    );
  });

  test("Another tenant's key sees only its own figures", async () => {
    const theirs = await lookup(tenantB.key);
    expect(theirs.status, theirs.text).toBe(200);
    // Tenant B spent nothing: its organization quota of ¥0.40 binds.
    expect(theirs.json.data.effective_remaining_usd, theirs.text).toBe("0.400000");
    expect(theirs.json.data.binding_level, theirs.text).toBe("organization");
    // A key of no member sees no balance and no organization quota.
    expect(theirs.json.data, theirs.text).not.toHaveProperty("balance");
    expect(theirs.json.data.budgets, theirs.text).toEqual([]);
    for (const figure of ["0.970000", "0.500000", "0.200000", "0.050000"]) {
      expect(theirs.text).not.toContain(figure);
    }
  });

  test("With the control plane unreachable, the lookup answers 503 and model traffic is untouched", async () => {
    relay!.pause();
    try {
      // Cached answers are served until they expire; the first lookup that
      // has to ask the control plane fails within the lookup's own deadline.
      let last = "no lookup";
      const failed = await pollUntil(
        "the owner's lookup to fail once its cached answer expired",
        async () => {
          const { answer, tookMs } = await timedLookup(ko);
          last = `${answer.status} after ${tookMs} ms: ${answer.text}`;
          return answer.status === 200
            ? undefined
            : { status: answer.status, tookMs, code: answer.json?.error?.code };
        },
        { timeoutMs: QUOTA_TTL_MS + ACCOUNT_TIMEOUT_MS + 2_000, intervalMs: 250, describe: () => last },
      );
      expect(failed.status, last).toBe(503);
      expect(failed.code, last).toBe("control_plane_unavailable");
      expect(failed.tookMs, last).toBeLessThanOrEqual(ACCOUNT_TIMEOUT_MS + 1_000);

      // While a lookup waits on the held control plane, a chat on the same key
      // is served, and waits on nothing the lookup waits on. A lookup that
      // returns at once was answered without asking the control plane, so
      // the pair is sent again until its lookup is one that waited.
      let pairLast = "no pair";
      const pair = await pollUntil(
        "a lookup that waits on the held control plane, beside a chat",
        async () => {
          const [lookup, chatted] = await Promise.all([timedLookup(ko), timedChat(ko)]);
          pairLast =
            `lookup ${lookup.answer.status} after ${lookup.tookMs} ms: ${lookup.answer.text}; ` +
            `chat ${chatted.answer.status} after ${chatted.tookMs} ms`;
          expect(chatted.answer.status, chatted.answer.text).toBe(200);
          expect(chatted.tookMs, pairLast).toBeLessThan(CHAT_DURING_OUTAGE_MS);
          return lookup.tookMs >= ACCOUNT_TIMEOUT_MS - 250 ? lookup : undefined;
        },
        { timeoutMs: 10_000, intervalMs: 300, describe: () => pairLast },
      );
      expect(pair.answer.status, pairLast).toBe(503);
      expect(pair.answer.json?.error?.code, pairLast).toBe("control_plane_unavailable");
      expect(pair.tookMs, pairLast).toBeLessThanOrEqual(ACCOUNT_TIMEOUT_MS + 1_000);
    } finally {
      relay!.resume();
    }
  });
});
