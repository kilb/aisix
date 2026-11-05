import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: two teams on one Provider Key do not see each other's files.
//
// The upstream account has one namespace, so what keeps team B out of team A's
// upload is the gateway. A key with a team or user owns what it creates: it can
// read it, delete it, cancel it, build a job from it, and read what that job
// produced. Anyone else addressing the same resource gets the answer an unknown
// id gets, and nothing reaches the provider. A list spans the whole account, so
// a key with an owner is told it cannot have one. A key with neither is the
// single-tenant shape and works as it always has.

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");
const TEAM_A = "sk-jobs-owner-team-a";
const TEAM_B = "sk-jobs-owner-team-b";
const USER_C = "sk-jobs-owner-user-c";
const SHARED = "sk-jobs-owner-shared";
const MODEL = "jobs-owner-model";

const FILE_BODY = { id: "file-up-1", object: "file", purpose: "batch", filename: "in.jsonl" };
const BATCH_BODY = {
  id: "batch-up-1",
  object: "batch",
  status: "in_progress",
  endpoint: "/v1/chat/completions",
  input_file_id: "file-up-1",
  output_file_id: "file-up-out",
};
const FT_BODY = {
  id: "ftjob-up-1",
  object: "fine_tuning.job",
  status: "running",
  training_file: "file-up-1",
  result_files: ["file-up-result"],
};

describe("jobs resources belong to the team or user that created them", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;
  // Minted by the gateway for team A, opaque to this test.
  let aFile = "";
  let aBatch = "";
  let aFtJob = "";
  let aOutputFile = "";
  let aResultFile = "";

  const request = (
    key: string,
    method: string,
    path: string,
    body?: unknown,
  ): Promise<Response> =>
    fetch(`${app!.proxyUrl}${path}`, {
      method,
      headers: {
        authorization: `Bearer ${key}`,
        ...(body ? { "content-type": "application/json" } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    });

  const upload = (key: string): Promise<Response> => {
    const form = new FormData();
    form.append("model", MODEL);
    form.append("purpose", "batch");
    form.append(
      "file",
      new Blob(['{"custom_id":"1"}\n'], { type: "application/jsonl" }),
      "in.jsonl",
    );
    return fetch(`${app!.proxyUrl}/v1/files`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}` },
      body: form,
    });
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      pathBodies: {
        "/v1/files": FILE_BODY,
        "/v1/files/file-up-1": FILE_BODY,
        "/v1/files/file-up-1/content": "team a's upload",
        "/v1/files/file-up-out/content": "team a's output",
        "/v1/files/file-up-result/content": "team a's result",
        "/v1/batches": BATCH_BODY,
        "/v1/batches/batch-up-1": BATCH_BODY,
        "/v1/batches/batch-up-1/cancel": { ...BATCH_BODY, status: "cancelling" },
        "/v1/fine_tuning/jobs": FT_BODY,
        "/v1/fine_tuning/jobs/ftjob-up-1": FT_BODY,
        "/v1/fine_tuning/jobs/ftjob-up-1/cancel": { ...FT_BODY, status: "cancelled" },
      },
    });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "jobs-owner-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: sha256(TEAM_A),
      allowed_models: [MODEL],
      team_id: "team-a",
    });
    await seed.createApiKey({
      key_hash: sha256(TEAM_B),
      allowed_models: [MODEL],
      team_id: "team-b",
    });
    await seed.createApiKey({
      key_hash: sha256(USER_C),
      allowed_models: [MODEL],
      user_id: "user-c",
    });
    await seed.createApiKey({ key_hash: sha256(SHARED), allowed_models: [MODEL] });

    await waitConfigPropagation(async () => {
      try {
        const results = await Promise.all(
          [TEAM_A, TEAM_B, USER_C, SHARED].map((key) => upload(key)),
        );
        return results.every((r) => r.status === 200);
      } catch {
        return false;
      }
    });

    aFile = ((await (await upload(TEAM_A)).json()) as { id: string }).id;
    const batch = (await (
      await request(TEAM_A, "POST", "/v1/batches", {
        input_file_id: aFile,
        endpoint: "/v1/chat/completions",
        completion_window: "24h",
      })
    ).json()) as { id: string; output_file_id: string };
    aBatch = batch.id;
    aOutputFile = batch.output_file_id;
    const job = (await (
      await request(TEAM_A, "POST", "/v1/fine_tuning/jobs", {
        training_file: aFile,
        model: "gpt-4o-mini",
      })
    ).json()) as { id: string; result_files: string[] };
    aFtJob = job.id;
    aResultFile = job.result_files[0];
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  const addressed = (): [string, string, unknown?][] => [
    ["GET", `/v1/files/${aFile}`],
    ["GET", `/v1/files/${aFile}/content`],
    ["GET", `/v1/files/${aOutputFile}/content`],
    ["GET", `/v1/files/${aResultFile}/content`],
    ["GET", `/v1/batches/${aBatch}`],
    ["POST", `/v1/batches/${aBatch}/cancel`],
    ["GET", `/v1/fine_tuning/jobs/${aFtJob}`],
    ["POST", `/v1/fine_tuning/jobs/${aFtJob}/cancel`],
    ["DELETE", `/v1/files/${aFile}`],
  ];

  test("the owner reaches what it created and what that produced", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const [method, path] of addressed()) {
      const r = await request(TEAM_A, method, path);
      expect(r.status, `${method} ${path}`).toBe(200);
      await r.text();
    }
    const output = await request(TEAM_A, "GET", `/v1/files/${aOutputFile}/content`);
    expect(await output.text()).toBe("team a's output");
    const result = await request(TEAM_A, "GET", `/v1/files/${aResultFile}/content`);
    expect(await result.text()).toBe("team a's result");
  });

  test("anyone else finds no such id, and the provider hears nothing", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const before = upstream.receivedRequests.length;
    for (const key of [TEAM_B, USER_C, SHARED]) {
      for (const [method, path] of addressed()) {
        const r = await request(key, method, path);
        expect(r.status, `${key} ${method} ${path}`).toBe(404);
        await r.text();
      }
    }
    expect(upstream.receivedRequests.length).toBe(before);
  });

  test("the provider's own id does not get a scoped key in", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const before = upstream.receivedRequests.length;
    for (const key of [TEAM_A, TEAM_B, USER_C]) {
      const r = await request(key, "GET", `/v1/files/file-up-1/content?model=${MODEL}`);
      expect(r.status, key).toBe(404);
      await r.text();
    }
    expect(upstream.receivedRequests.length).toBe(before);
  });

  test("a job cannot be built on someone else's file", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const bFile = ((await (await upload(TEAM_B)).json()) as { id: string }).id;
    const before = upstream.receivedRequests.length;
    const refused: [string, unknown][] = [
      [
        "/v1/batches",
        { input_file_id: aFile, endpoint: "/v1/chat/completions", completion_window: "24h" },
      ],
      ["/v1/fine_tuning/jobs", { training_file: aFile, model: "gpt-4o-mini" }],
      [
        "/v1/fine_tuning/jobs",
        { training_file: bFile, validation_file: aFile, model: "gpt-4o-mini" },
      ],
      // The provider's own id names no one, so it is not this key's either.
      [
        `/v1/batches?model=${MODEL}`,
        { input_file_id: "file-up-1", endpoint: "/v1/chat/completions", completion_window: "24h" },
      ],
      [`/v1/fine_tuning/jobs?model=${MODEL}`, { training_file: "file-up-1", model: "gpt-4o-mini" }],
      [
        "/v1/fine_tuning/jobs",
        { training_file: bFile, validation_file: "file-up-1", model: "gpt-4o-mini" },
      ],
    ];
    for (const [path, body] of refused) {
      const r = await request(TEAM_B, "POST", path, body);
      expect(r.status, JSON.stringify(body)).toBe(404);
      await r.text();
    }
    expect(upstream.receivedRequests.length).toBe(before);
  });

  test("a key with an owner is told it cannot list, and why", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const before = upstream.receivedRequests.length;
    for (const path of ["/v1/files", "/v1/batches", "/v1/fine_tuning/jobs"]) {
      for (const key of [TEAM_A, USER_C]) {
        const r = await request(key, "GET", `${path}?model=${MODEL}`);
        expect(r.status, `${key} ${path}`).toBe(403);
        const body = (await r.json()) as { error: { message: string } };
        expect(body.error.message).toMatch(/list/i);
        expect(body.error.message).toMatch(/team or user/i);
      }
    }
    expect(upstream.receivedRequests.length).toBe(before);
  });

  test("a key with no owner stays as it was", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const own = ((await (await upload(SHARED)).json()) as { id: string }).id;
    for (const path of [
      `/v1/files/${own}/content`,
      `/v1/files/file-up-1/content?model=${MODEL}`,
      `/v1/files?model=${MODEL}`,
      `/v1/batches?model=${MODEL}`,
      `/v1/fine_tuning/jobs?model=${MODEL}`,
    ]) {
      const r = await request(SHARED, "GET", path);
      expect(r.status, path).toBe(200);
      await r.text();
    }
    const created = await request(SHARED, "POST", `/v1/batches?model=${MODEL}`, {
      input_file_id: "file-up-1",
      endpoint: "/v1/chat/completions",
      completion_window: "24h",
    });
    expect(created.status).toBe(200);
    await created.text();
  });
});
