import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type IssuedGateway,
  type Json,
  type ManagedGateway,
} from "../harness/control-plane.js";
import { callGateway } from "../harness/cp-upstream.js";
import { scrapeMetrics } from "../harness/metrics.js";
import { ProxyClient } from "../harness/proxy.js";
import { startSwallowRelay, type SwallowRelay } from "../harness/swallow-relay.js";
import { startOpenAiUpstream, type OpenAiUpstream } from "../harness/upstream-openai.js";

// E2E: usage a gateway cannot deliver is not lost silently. The gateway
// counts every usage event it drops, by reason, reports those counts in its
// heartbeat, and the control plane shows them on the gateway's instance;
// together with the rows the control plane stored, every request is
// accounted for.
//
// The gateway's REST link to the control plane (`cp_base_url`, which carries
// both the usage deliveries and the heartbeats) runs through a relay that can
// hold every connection; its etcd link goes straight to the control plane,
// so configuration keeps flowing while the relay holds usage back.

const MODEL = "usage-loss-chat-e2e";
const DROPS = "aisix_usage_event_drops_total";
const CHUNK = 2_000;
const CONCURRENCY = 64;
const MAX_PAUSED_SENDS = 40_000;
const chat = { model: MODEL, messages: [{ role: "user", content: "hi" }] };

describe("usage the gateway cannot deliver", () => {
  let cp: ControlPlaneStack | undefined;
  let upstream: OpenAiUpstream | undefined;
  let relay: SwallowRelay | undefined;
  let gw: IssuedGateway;
  let app: ManagedGateway | undefined;
  let keyId: string;
  let key: string;

  /** The drops the gateway's own metric shows, summed by `reason`. */
  const metricDrops = async (): Promise<Record<string, number>> => {
    const byReason: Record<string, number> = {};
    for (const s of await scrapeMetrics(app!.metricsUrl)) {
      if (s.name !== DROPS) continue;
      const reason = s.labels.reason ?? "";
      byReason[reason] = (byReason[reason] ?? 0) + s.value;
    }
    return byReason;
  };

  /** Every usage row the control plane stored for the caller key. */
  const storedRows = async (): Promise<number> => {
    let count = 0;
    let before: number | null = null;
    for (;;) {
      const res = await cp!.admin.get(
        `/admin/usage/events?limit=500&api_key_id=${keyId}${before === null ? "" : `&before=${before}`}`,
      );
      expect(res.status, res.text).toBe(200);
      count += (res.body.items as Json[]).length;
      before = res.body.next_before;
      if (before === null) return count;
    }
  };

  /** One chat; returns the request id the gateway answered with. */
  const sendOne = async (): Promise<string> => {
    const res = await callGateway(app!.proxyUrl, key, "/v1/chat/completions", chat);
    expect(res.status, res.text).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id");
    expect(requestId).toBeTruthy();
    return requestId!;
  };

  /** Whether the control plane stored the row of `requestId`. */
  const isStored = async (requestId: string): Promise<boolean> => {
    const res = await cp!.admin.get(`/admin/usage/events?limit=500&request_id=${requestId}`);
    expect(res.status, res.text).toBe(200);
    return (res.body.items as Json[]).length >= 1;
  };

  /** The `seq` of the newest usage row stored for the caller key, `null` before any. */
  const newestSeq = async (): Promise<number | null> => {
    const res = await cp!.admin.get(`/admin/usage/events?limit=1&api_key_id=${keyId}`);
    expect(res.status, res.text).toBe(200);
    return (res.body.items as Json[])[0]?.seq ?? null;
  };

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    cp = await startControlPlane();
    relay = await startSwallowRelay(new URL(cp.dpUrl).host);
    gw = await cp.issueGateway();
    app = await spawnManagedGateway(gw, {
      managed: { cp_base_url: `https://127.0.0.1:${relay.port}`, heartbeat_interval_secs: 5 },
    });
    await cp.admin.waitRegistered(gw.dpId);

    // The platform's own key below names the group it reaches, so this case
    // creates one and puts the account in it beside `shared`.
    const group = await cp.admin.createGroup("grp-usage-loss-7c1", "用量丢失 7c1");
    const account = await cp.admin.create("provider_keys", {
      display_name: "usage-loss-account-e2e",
      provider: "openai",
      api_key: "sk-usage-loss-upstream-e2e",
      api_base: `${upstream.baseUrl}/v1`,
      groups: ["shared", "grp-usage-loss-7c1"],
    });
    await cp.admin.create("models", {
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: account.id,
    });
    // The platform's own tenant: no balance, credit limit or quota stands
    // between the case and the volume it needs.
    const tenants = await cp.admin.get("/admin/tenants");
    expect(tenants.status, tenants.text).toBe(200);
    const internal = (tenants.body.items as Json[]).find((t) => t.is_internal);
    expect(internal, tenants.text).toBeDefined();
    // Every key names one group; the platform's own tenant has no
    // assignment list, so it names the one above.
    const minted = await cp.admin.post(`/admin/tenants/${internal.id}/keys`, {
      label: "usage-loss-caller",
      allowed_models: ["*"],
      group_id: group.id,
    });
    expect(minted.status, minted.text).toBe(201);
    keyId = minted.body.key.id;
    key = minted.body.plaintext;

    const proxy = new ProxyClient(app.proxyUrl, key);
    let last = 0;
    await pollUntil(
      "the caller key to authenticate",
      async () => ((last = (await proxy.listModels()).status) === 200 ? true : undefined),
      { describe: () => `last status ${last}` },
    );
  });

  afterAll(async () => {
    await app?.exit();
    await relay?.close();
    await cp?.stop();
    await upstream?.close();
  });

  test(
    "Usage the gateway cannot deliver is counted as dropped, and every request is accounted for within the gateway's own bound",
    async () => {
      let sent = 0;
      /** `n` chats, `CONCURRENCY` at a time; every one must be answered 200. */
      const sendMany = async (n: number) => {
        let started = 0;
        const failures: string[] = [];
        await Promise.all(
          Array.from({ length: CONCURRENCY }, async () => {
            while (started < n) {
              started++;
              sent++;
              const res = await callGateway(app!.proxyUrl, key, "/v1/chat/completions", chat);
              if (res.status !== 200 && failures.length < 5) failures.push(`${res.status}: ${res.text}`);
            }
          }),
        );
        expect(failures, "every chat is answered").toEqual([]);
      };

      // A delivered, deduplicated answer first, so the gateway knows it may
      // re-send and holds a pooled connection to the control plane.
      sent++;
      const first = await sendOne();
      await pollUntil(`the usage row of ${first}`, async () => ((await isStored(first)) ? true : undefined), {
        timeoutMs: 60_000,
      });

      relay!.pause();
      let pausedSends = 0;
      let sinkFull = 0;
      while (sinkFull === 0) {
        expect(
          pausedSends,
          `${pausedSends} chats sent with the control plane unreachable and no usage event dropped as sink_full`,
        ).toBeLessThan(MAX_PAUSED_SENDS);
        await sendMany(CHUNK);
        pausedSends += CHUNK;
        sinkFull = (await metricDrops()).sink_full ?? 0;
      }
      relay!.resume();

      // A barrier: once its row is stored, every event queued ahead of it
      // has been delivered or dropped. The queue can still be full when the
      // barrier is sent, and then the barrier itself is dropped, a
      // `sink_full` nothing else can cause now: wait until the gateway
      // delivers more, and send another. Every barrier counts in `sent`.
      const deadline = Date.now() + 300_000;
      const remaining = () => Math.max(deadline - Date.now(), 1);
      await (async () => {
        for (;;) {
          const newest = await newestSeq();
          const sinkFullBefore = (await metricDrops()).sink_full ?? 0;
          sent++;
          const barrier = await sendOne();
          const outcome = await pollUntil(
            `the usage row of barrier ${barrier}, or its drop`,
            async () => {
              if (await isStored(barrier)) return "stored";
              return ((await metricDrops()).sink_full ?? 0) > sinkFullBefore ? "dropped" : undefined;
            },
            { timeoutMs: remaining() },
          );
          if (outcome === "stored") return;
          await pollUntil(
            "the gateway to deliver more usage",
            async () => ((await newestSeq()) !== newest ? true : undefined),
            { timeoutMs: remaining() },
          );
        }
      })().catch(async (e: Error) => {
        const soFar = `${await storedRows()} of ${sent} stored, drops ${JSON.stringify(await metricDrops())}`;
        throw new Error(`${e.message} (${soFar})`);
      });

      let metric: Record<string, number> = {};
      let reported: unknown;
      await pollUntil(
        "the instance's reported usage drops to equal the gateway's metric",
        async () => {
          metric = await metricDrops();
          reported = (await cp!.admin.instance(gw.dpId))?.usage_event_drops;
          return JSON.stringify(sortKeys(reported)) === JSON.stringify(sortKeys(metric)) ? true : undefined;
        },
        {
          timeoutMs: 15_000,
          describe: () => `reported ${JSON.stringify(reported)}, metric ${JSON.stringify(metric)}`,
        },
      );
      expect(metric.sink_full ?? 0).toBeGreaterThan(0);
      expect(reported).toEqual(metric);

      const dropped = Object.values(metric).reduce((a, b) => a + b, 0);
      const unconfirmed = metric.send_unconfirmed ?? 0;
      const stored = await storedRows();
      const accounting = `sent ${sent}, stored ${stored}, dropped ${JSON.stringify(metric)}`;
      // No delivery here is given up on as possibly stored: the first chat's
      // answer told the gateway the control plane de-duplicates, so a send
      // the relay held unanswered is re-sent, never abandoned, well inside
      // this case's time. An unconfirmed drop would be a gateway bug, so
      // the accounting is exact.
      expect(unconfirmed, accounting).toBe(0);
      expect(stored + dropped, accounting).toBe(sent);
    },
    600_000,
  );
});

/** `value` with its keys in order, when it is a plain object. */
function sortKeys(value: unknown): unknown {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return value;
  return Object.fromEntries(Object.entries(value as Record<string, unknown>).sort(([a], [b]) => a.localeCompare(b)));
}
