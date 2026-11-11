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
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a proxied video download is settled by how its BODY ends.
//
// R-107: `GET /v1/videos/:id/content` for a Proxy-delivery provider streams
// the provider's bytes back. The 200 head only proves the upstream answered
// the GET — a body that stalls or ends short is the upstream failing the
// download — so the outcome the target's health, cooldown and the
// `aisix_deployment_*` counters see has to come from the body's own ending:
// a complete transfer is a success, a timeout or a transport failure is a
// failure, and a caller who leaves mid-download is a cancellation that says
// neither. Settling at the head recorded every one of them as a success.
//
// The HTTP status is NOT rewritten: the head has gone out, and a truncated
// `video/mp4` is the caller's signal that the transfer failed.

const CALLER = "sk-videos-content-settlement";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const PROVIDER_SECRET = "sk-content-settlement-provider";
const READ_TIMEOUT_MS = 300;
const STALL_MS = 3_000;
const FULL_BODY = "0123456789ABCDEFGHIJ";
const HALF_BODY = FULL_BODY.slice(0, 10);

describe("a proxied video download is settled by its body's ending", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  const upstreams: OpenAiUpstream[] = [];
  const modelIds: Record<string, string> = {};

  const completed = (id: string) => ({
    nonStreamBody: {
      id,
      object: "video",
      model: "sora-2-mock",
      status: "completed",
      progress: 100,
    },
  });

  const syntheticId = (modelId: string, alias: string, task: string) =>
    Buffer.from(
      `${modelId}:${Buffer.from(alias).toString("base64url")}:${task}`,
    ).toString("base64url");

  const content = (model: string, modelId: string, task = "download") =>
    fetch(
      `${app!.proxyUrl}/v1/videos/${syntheticId(modelId, model, task)}/content`,
      { headers: { authorization: `Bearer ${CALLER}` } },
    );

  /** Read a body to its end; a body cut off short may end in a read error. */
  const drain = async (res: Response) => {
    const reader = res.body!.getReader();
    const decoder = new TextDecoder();
    let body = "";
    let failed = false;
    try {
      for (;;) {
        const next = await reader.read();
        if (next.done) break;
        body += decoder.decode(next.value, { stream: true });
      }
    } catch {
      failed = true;
    }
    return { body: body + decoder.decode(), failed };
  };

  const settlement = async (model: string) => {
    const samples = await scrapeMetrics(app!.metricsUrl);
    return {
      success: samples
        .filter(
          (s) =>
            s.name === "aisix_deployment_success_responses_total" &&
            s.labels.model === model,
        )
        .reduce((acc, s) => acc + s.value, 0),
      failure: samples
        .filter(
          (s) =>
            s.name === "aisix_deployment_failure_responses_total" &&
            s.labels.model === model,
        )
        .reduce((acc, s) => acc + s.value, 0),
    };
  };

  const modelStatus = async (modelId: string) => {
    const res = await fetch(`${app!.metricsUrl}/status/models`);
    if (res.status !== 200) {
      throw new Error(`model status probe returned ${res.status}`);
    }
    const rows = (await res.json()) as Array<{
      id?: string;
      status?: string;
      status_reason?: string;
    }>;
    return rows.find((row) => row.id === modelId);
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    // Each model's upstream answers the content route's own poll with a
    // completed task, then serves the content GET.
    const downstream = async (body: Parameters<typeof startOpenAiUpstream>[0]) => {
      const upstream = await startOpenAiUpstream(body);
      upstreams.push(upstream);
      return upstream;
    };
    const complete = await downstream({
      scriptedResponses: [
        completed("video_full"),
        { rawBody: FULL_BODY, rawContentType: "video/mp4" },
      ],
    });
    // Declares twenty bytes, sends ten, then closes: the download the caller
    // can see did not finish.
    const short = await downstream({
      scriptedResponses: [
        completed("video_short"),
        {
          rawBodyChunks: [HALF_BODY],
          rawContentType: "video/mp4",
          responseHeaders: { "content-length": String(FULL_BODY.length) },
          disconnectAfterChunks: true,
        },
      ],
    });
    // Sends ten bytes, then stalls well past the model's stream budget.
    const stalled = await downstream({
      scriptedResponses: [
        completed("video_stalled"),
        {
          rawBodyChunks: [HALF_BODY, FULL_BODY.slice(10)],
          rawContentType: "video/mp4",
          eventDelayMs: STALL_MS,
        },
      ],
    });

    const seedModel = async (
      name: string,
      upstream: OpenAiUpstream,
      streamTimeout?: number,
    ) => {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: PROVIDER_SECRET,
        api_base: upstream.baseUrl,
        provider: "openai",
      });
      const model = await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "sora-2-mock",
        provider_key_id: pk.id,
        ...(streamTimeout === undefined ? {} : { stream_timeout: streamTimeout }),
        cooldown: { enabled: true, default_seconds: 60 },
      });
      modelIds[name] = model.id;
    };
    // A body big enough that the caller can hang up after its first chunk,
    // with an upstream that keeps writing: every request against it gets the
    // same completed poll and the same bytes, so the slot the cancelled
    // download held can be asked for again.
    const trickle = (task: string) => ({
      scriptedResponses: [
        // One scripted sequence per download: the content route polls first,
        // then fetches the bytes.
        completed(task),
        {
          rawBodyChunks: [FULL_BODY, FULL_BODY.repeat(64)],
          rawContentType: "video/mp4",
          eventDelayMs: 1_000,
        },
        completed(task),
        {
          rawBodyChunks: [FULL_BODY, FULL_BODY.repeat(64)],
          rawContentType: "video/mp4",
          eventDelayMs: 1_000,
        },
      ],
    });
    const slow = await downstream(trickle("video_slow"));
    // A body whose length IS declared, written slowly: the caller can hang up
    // after the first chunk with the transfer legitimately incomplete. A
    // declared length must not turn that abandonment into the target's
    // failure — the witness for a truncated transfer is the transport's read
    // error (the `content-short` case), never "fewer bytes than promised".
    const sizedSlow = (task: string) => ({
      scriptedResponses: [
        completed(task),
        {
          rawBodyChunks: [HALF_BODY, FULL_BODY.slice(10)],
          rawContentType: "video/mp4",
          responseHeaders: { "content-length": String(FULL_BODY.length) },
          eventDelayMs: 1_000,
        },
        completed(task),
        {
          rawBodyChunks: [HALF_BODY, FULL_BODY.slice(10)],
          rawContentType: "video/mp4",
          responseHeaders: { "content-length": String(FULL_BODY.length) },
          eventDelayMs: 1_000,
        },
      ],
    });
    const sized = await downstream(sizedSlow("video_sized"));
    await seedModel("content-complete", complete);
    await seedModel("content-cancelled", slow);
    await seedModel("content-cancelled-sized", sized);
    await seedModel("content-short", short);
    await seedModel("content-stalled", stalled, READ_TIMEOUT_MS);

    // Seeded last: once it authenticates, every resource above is live.
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: [
        "content-complete",
        "content-short",
        "content-stalled",
        "content-cancelled",
        "content-cancelled-sized",
      ],
    });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      await res.text();
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  test("a complete download settles as a deployment success", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = await settlement("content-complete");
    const res = await content("content-complete", modelIds["content-complete"]!);
    expect(res.status).toBe(200);
    const { body, failed } = await drain(res);
    expect(failed).toBe(false);
    expect(body).toBe(FULL_BODY);
    // Two samples belong to the request: the content route's own poll, which
    // any follow-up round-trip books, and the download's own ending.
    await waitConfigPropagation(
      async () => (await settlement("content-complete")).success === before.success + 2,
    );
    expect((await settlement("content-complete")).failure).toBe(before.failure);
    expect((await modelStatus(modelIds["content-complete"]!))?.status).not.toBe(
      "cooldown",
    );
  }, 30_000);

  test("a body that ends short of its declared length fails the download", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = await settlement("content-short");
    const res = await content("content-short", modelIds["content-short"]!);
    // The head already went out; the truncation is the caller's signal.
    expect(res.status).toBe(200);
    const { body, failed } = await drain(res);
    // Whatever arrived of the ten bytes, the transfer is dead: the caller's
    // read fails and it never has the twenty the upstream declared.
    expect(body.length).toBeLessThan(FULL_BODY.length);
    expect(FULL_BODY.startsWith(body)).toBe(true);
    expect(failed).toBe(true);
    await waitConfigPropagation(
      async () => (await settlement("content-short")).failure === before.failure + 1,
    );
    // The poll answered; the transfer did not.
    expect((await settlement("content-short")).success).toBe(before.success + 1);
  }, 30_000);

  test("a body that stalls mid-download fails it and cools the target down", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = await settlement("content-stalled");
    const res = await content("content-stalled", modelIds["content-stalled"]!);
    expect(res.status).toBe(200);
    const { body } = await drain(res);
    expect(body).toBe(HALF_BODY);
    await waitConfigPropagation(async () => {
      const row = await modelStatus(modelIds["content-stalled"]!);
      return row?.status === "cooldown" && row.status_reason === "request_timeout";
    });
    await waitConfigPropagation(
      async () => (await settlement("content-stalled")).failure === before.failure + 1,
    );
    expect((await settlement("content-stalled")).success).toBe(before.success + 1);
  }, 30_000);

  test("a caller that leaves mid-download settles neither way and frees its slot", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = await settlement("content-cancelled");
    const res = await content("content-cancelled", modelIds["content-cancelled"]!);
    expect(res.status).toBe(200);
    const reader = res.body!.getReader();
    const first = await reader.read();
    expect(first.done).toBe(false);
    await reader.cancel();

    // A cancellation is not an answer: nothing is booked either way.
    await new Promise((resolve) => setTimeout(resolve, 500));
    const after = await settlement("content-cancelled");
    // The poll answered — the only sample this request books.
    expect(after.success).toBe(before.success + 1);
    expect(after.failure).toBe(before.failure);

    // ...and the slot the download held is gone, not still held for a body
    // nobody is reading: the next download of the same key is served.
    const next = await content("content-cancelled", modelIds["content-cancelled"]!, "again");
    expect(next.status).toBe(200);
    const reader2 = next.body!.getReader();
    expect((await reader2.read()).done).toBe(false);
    await reader2.cancel();
    expect((await modelStatus(modelIds["content-cancelled"]!))?.status).not.toBe(
      "cooldown",
    );
  }, 30_000);

  test("a caller that leaves a download whose length WAS declared settles neither way either", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = await settlement("content-cancelled-sized");
    const res = await content(
      "content-cancelled-sized",
      modelIds["content-cancelled-sized"]!,
    );
    expect(res.status).toBe(200);
    const reader = res.body!.getReader();
    const first = await reader.read();
    expect(first.done).toBe(false);
    await reader.cancel();

    // Fewer bytes than the upstream declared, but nobody failed: the caller
    // left. A fabricated failure here would cool down a healthy target after
    // a handful of abandoned downloads, which is what this case pins.
    await new Promise((resolve) => setTimeout(resolve, 500));
    const after = await settlement("content-cancelled-sized");
    expect(after.success).toBe(before.success + 1);
    expect(after.failure).toBe(before.failure);
    expect((await modelStatus(modelIds["content-cancelled-sized"]!))?.status).not.toBe(
      "cooldown",
    );
  }, 30_000);
});
