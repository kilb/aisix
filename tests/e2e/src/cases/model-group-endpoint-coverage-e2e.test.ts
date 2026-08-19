import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  metricDelta,
  scrapeMetrics,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MetricSample,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// A Model Group is one resource, and an operator who builds one for a model
// family reasonably expects every endpoint that takes a model to dispatch
// it. Five did not: embeddings, rerank, completions, images and audio each
// resolved the caller-addressed row and handed it straight to a provider,
// so a group arrived at `require_provider` and came back
//
//   model "<group>" has no provider_key_id (routing models can't be
//   dispatched directly)
//
// — the exact 400 that #471 fixed for /v1/messages, still live on the
// endpoints nobody had swept. The failure mode is worse than a plain gap:
// the same configuration works on chat and 400s here, which reads as a
// gateway bug rather than an unimplemented feature.
//
// Each case below points its group at a dead target first and a live one
// second, so a 200 can only come from a real fall-over. The assertions
// cover the three things the shared walk owes a caller:
//
//   1. the request succeeds from the surviving target;
//   2. the fall-over is visible in `aisix_routing_successful_fallbacks_total`;
//   3. per-target health is attributed to the member — the dead one
//      accumulates deployment failures, the live one successes — because
//      that is what later excludes it from the candidate set.

const CALLER_PLAINTEXT = "sk-mg-endpoints-e2e";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

interface Case {
  /** Group name, and the two member rows behind it. */
  group: string;
  dead: string;
  live: string;
  path: string;
  /** 200 body the live upstream returns. */
  liveBody: unknown;
  /** Request the caller sends. */
  request: () => Promise<{ status: number; text: string }>;
}

describe("model group endpoint coverage e2e", () => {
  let app: SpawnedApp | undefined;
  let dead: OpenAiUpstream | undefined;
  let live: OpenAiUpstream | undefined;
  let etcdReachable = false;

  // One dead upstream and one live one, shared by every case: the live mock
  // answers any path with a body shaped for that path, so a single pair
  // covers all five endpoints.
  const GROUPS = [
    { group: "mg-embed", path: "/v1/embeddings" },
    { group: "mg-rerank", path: "/v1/rerank" },
    { group: "mg-cmpl", path: "/v1/completions" },
    { group: "mg-image", path: "/v1/images/generations" },
    { group: "mg-audio", path: "/v1/audio/speech" },
  ] as const;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // 503 on every path — the first target of every group.
    dead = await startOpenAiUpstream({ status: 503, errorBody: { error: "overloaded" } });
    live = await startOpenAiUpstream({ pathBodies: pathBodies() });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const deadPk = await seed.createProviderKey({
      display_name: "mg-dead-pk",
      secret: "sk-mock",
      api_base: `${dead.baseUrl}/v1`,
    });
    const livePk = await seed.createProviderKey({
      display_name: "mg-live-pk",
      secret: "sk-mock",
      api_base: `${live.baseUrl}/v1`,
    });

    const names: string[] = [];
    for (const { group } of GROUPS) {
      for (const [suffix, pk] of [
        ["dead", deadPk.id],
        ["live", livePk.id],
      ] as const) {
        await seed.createModel({
          display_name: `${group}-${suffix}`,
          provider: "openai",
          model_name: "gpt-4o-mini",
          provider_key_id: pk,
          // Cooldown off: several cases share the dead row's provider key
          // and a cooldown mark from an earlier case would remove the
          // target a later case is trying to fail over FROM.
          cooldown: { enabled: false },
        });
      }
      await seed.createModel({
        display_name: group,
        routing: {
          targets: [{ model: `${group}-dead` }, { model: `${group}-live` }],
        },
      });
      names.push(group, `${group}-dead`, `${group}-live`);
    }
    // Last, so a gate on this key authenticating implies every row above.
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: names });
  });

  afterAll(async () => {
    await app?.exit();
    await dead?.close();
    await live?.close();
  });

  test("every model-taking endpoint dispatches a group and fails over", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => {
      const r = await post("/v1/embeddings", { model: "mg-embed", input: "ready" });
      return r.status === 200;
    });

    const requests: Record<string, unknown> = {
      "/v1/embeddings": { model: "mg-embed", input: "hi" },
      "/v1/rerank": {
        model: "mg-rerank",
        query: "hi",
        documents: ["a", "b"],
      },
      "/v1/completions": { model: "mg-cmpl", prompt: "hi" },
      "/v1/images/generations": { model: "mg-image", prompt: "a cat" },
      "/v1/audio/speech": {
        model: "mg-audio",
        input: "hello",
        voice: "alloy",
      },
    };

    const failed: string[] = [];
    const noFallbackMetric: string[] = [];
    const noMemberHealth: string[] = [];

    for (const { group, path } of GROUPS) {
      const before = await scrape(app);
      const res = await post(path, requests[path]);
      if (res.status !== 200) {
        failed.push(`${path} (${group}): ${res.status} ${res.text.slice(0, 200)}`);
        continue;
      }
      const after = await scrape(app);

      // The fall-over is labelled by the GROUP the caller asked for — that
      // is the series an operator watches to see a group leaning on its
      // backups.
      if (
        metricDelta(before, after, "aisix_routing_successful_fallbacks_total", (l) =>
          Object.values(l).includes(group),
        ) < 1
      ) {
        noFallbackMetric.push(`${path} (${group})`);
      }

      // Per-target health: the member that answered gets the success, the
      // member that 503'd gets the failure. Attributing either to the group
      // would make the group itself look unhealthy and take every member
      // down with it.
      const liveSuccess = metricDelta(
        before,
        after,
        "aisix_deployment_success_responses_total",
        (l) => Object.values(l).includes(`${group}-live`),
      );
      const deadFailure = metricDelta(
        before,
        after,
        "aisix_deployment_failure_responses_total",
        (l) => Object.values(l).includes(`${group}-dead`),
      );
      if (liveSuccess < 1 || deadFailure < 1) {
        noMemberHealth.push(
          `${path} (${group}): live success +${liveSuccess}, dead failure +${deadFailure}`,
        );
      }
    }

    expect(
      failed,
      `these endpoints refused a Model Group the same configuration serves on ` +
        `/v1/chat/completions:\n` + failed.map((f) => `  ${f}`).join("\n"),
    ).toEqual([]);
    expect(
      noFallbackMetric,
      `these endpoints fell over without recording it, so a group leaning on ` +
        `its backups is invisible:\n` +
        noFallbackMetric.map((f) => `  ${f}`).join("\n"),
    ).toEqual([]);
    expect(
      noMemberHealth,
      `these endpoints did not attribute health per member, so one bad target ` +
        `cannot be singled out:\n` + noMemberHealth.map((f) => `  ${f}`).join("\n"),
    ).toEqual([]);
  }, 180_000);

  async function post(
    path: string,
    body: unknown,
  ): Promise<{ status: number; text: string }> {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  }
});

async function scrape(app: SpawnedApp | undefined): Promise<MetricSample[]> {
  return scrapeMetrics(app!.metricsUrl);
}

/** Per-path 200 bodies for the surviving upstream. */
function pathBodies(): Record<string, unknown> {
  return {
    "/v1/embeddings": {
      object: "list",
      model: "gpt-4o-mini",
      data: [{ object: "embedding", index: 0, embedding: [0.1, 0.2] }],
      usage: { prompt_tokens: 4, total_tokens: 4 },
    },
    "/v1/rerank": {
      id: "rerank-mg",
      results: [{ index: 0, relevance_score: 0.9 }],
      usage: { prompt_tokens: 5, total_tokens: 5 },
    },
    "/v1/completions": {
      object: "text_completion",
      model: "gpt-4o-mini",
      choices: [{ text: "ok", index: 0, finish_reason: "stop" }],
      usage: { prompt_tokens: 2, completion_tokens: 1, total_tokens: 3 },
    },
    "/v1/images/generations": {
      created: 0,
      data: [{ url: "https://example.invalid/cat.png" }],
    },
    "/v1/audio/speech": "AUDIO-BYTES",
  };
}
