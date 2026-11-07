import { spawn, type ChildProcess } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { stringify as yamlStringify } from "yaml";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  pickFreePorts,
  recordProfileOwner,
  spawnApp,
  startOpenAiUpstream,
  stopGateway,
  suiteThreadPerCore,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const BIN_PATH =
  process.env.AISIX_BIN ?? join(process.cwd(), "..", "..", "target", "debug", "aisix");

// E2E for the load-observability contract: `GET /status/config`,
// `GET /status/ready`, and the `aisix_config_*` Prometheus series on the
// dedicated metrics/status listener. Exercises both load paths (etcd watch
// and standalone file) against the real binary — the endpoints report the
// gateway's own internal load state, so these are source-blind black-box
// checks of the observable contract.

const CALLER_PLAINTEXT = "sk-status-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

interface StatusConfig {
  state: string;
  source: {
    type: string;
    connected?: boolean;
    observed_revision?: number;
    source_hash?: string;
    observed_at?: string;
  };
  applied?: {
    applied_revision?: number;
    config_hash: string;
    apply_seq: number;
    applied_at: string;
    resource_counts: Record<string, number>;
  };
  last_reload?: { successful: boolean; at: string };
  last_failure: { at: string; last_error_kind: string; last_error: string } | null;
  rejected: Array<{
    resource_kind: string;
    resource_id: string;
    last_error_kind: string;
    last_error: string;
    serving_stale_since?: string;
    first_seen_at: string;
    last_seen_at: string;
  }>;
  partially_compatible: Array<{
    resource_kind: string;
    field: string;
    count: number;
  }>;
}

async function getStatusConfig(app: SpawnedApp): Promise<StatusConfig> {
  const res = await fetch(`${app.metricsUrl}/status/config`);
  expect(res.status).toBe(200);
  return (await res.json()) as StatusConfig;
}

async function scrape(app: SpawnedApp): Promise<string> {
  const res = await fetch(`${app.metricsUrl}/metrics`);
  expect(res.status).toBe(200);
  return res.text();
}

/** Value of a single-sample (no-label) gauge line from a scrape. */
function gaugeValue(scrapeText: string, metric: string): number | undefined {
  for (const line of scrapeText.split("\n")) {
    if (line.startsWith(`${metric} `)) {
      const v = Number(line.slice(metric.length + 1).trim());
      if (!Number.isNaN(v)) return v;
    }
  }
  return undefined;
}

function configHashValue(scrapeText: string): string | undefined {
  return scrapeText.match(/aisix_config_hash_info\{hash="([0-9a-f]{64})"\} 1/)?.[1];
}

describe("status/config: etcd watch source", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream();
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "status-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "status-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["status-model"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("clean config reports synced with revisions and resource counts", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    let cfg: StatusConfig | undefined;
    // "synced" alone races the seed: the state flips synced on an EARLY
    // revision before all three seeded resources are applied, and the
    // count asserts below then read undefined. Wait until the applied
    // snapshot actually contains the seeded resources.
    await waitConfigPropagation(async () => {
      cfg = await getStatusConfig(app!);
      return (
        cfg.state === "synced" &&
        cfg.applied?.resource_counts.models === 1 &&
        cfg.applied?.resource_counts.provider_keys === 1 &&
        cfg.applied?.resource_counts.api_keys === 1
      );
    });

    expect(cfg!.source.type).toBe("etcd");
    expect(cfg!.source.connected).toBe(true);
    // observed >= applied revision, both present in etcd mode.
    expect(typeof cfg!.source.observed_revision).toBe("number");
    expect(typeof cfg!.applied?.applied_revision).toBe("number");
    expect(cfg!.source.observed_revision!).toBeGreaterThanOrEqual(
      cfg!.applied!.applied_revision!,
    );
    expect(cfg!.source.source_hash).toMatch(/^[0-9a-f]{64}$/);
    expect(cfg!.applied!.config_hash).toMatch(/^[0-9a-f]{64}$/);
    // Clean load: applied hash equals the observed source hash.
    expect(cfg!.applied!.config_hash).toBe(cfg!.source.source_hash);
    expect(cfg!.applied!.resource_counts.models).toBe(1);
    expect(cfg!.applied!.resource_counts.provider_keys).toBe(1);
    expect(cfg!.applied!.resource_counts.api_keys).toBe(1);
    expect(cfg!.last_reload?.successful).toBe(true);
    expect(cfg!.last_failure).toBeNull();
    expect(cfg!.rejected).toHaveLength(0);
  });

  test("metrics listener exposes aisix_config_* series", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await getStatusConfig(app!)).state === "synced");

    const text = await scrape(app);
    expect(gaugeValue(text, "aisix_config_last_reload_successful")).toBe(1);
    expect(gaugeValue(text, "aisix_config_source_connected")).toBe(1);
    expect(text).toContain("aisix_config_last_reload_success_timestamp_seconds");
    expect(text).toContain("aisix_config_reloads_total");
    expect(text).toMatch(/aisix_config_observed_revision \d+/);
    expect(text).toMatch(/aisix_config_applied_revision \d+/);
    expect(text).toMatch(/aisix_config_hash_info\{hash="[0-9a-f]{64}"\} 1/);
  });

  // The gateway computes the configuration digests when something reports
  // them, not on every apply, so what a reader gets back is produced after
  // the fact from the state the gateway holds. Two failures that shape
  // allows and nothing else here would catch: a digest that never moves
  // once it has been computed, and one that moves while the configuration
  // sits still. Both surfaces have to agree with each other throughout —
  // `/status/config` builds its answer in the handler, the scrape streams
  // its copy out of the recorder.
  test("the reported digest moves only when the configuration does", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await getStatusConfig(app!)).state === "synced");

    const before = await getStatusConfig(app);
    expect(configHashValue(await scrape(app))).toBe(before.applied!.config_hash);

    // Nothing changed: reading again re-reports the same digest rather
    // than a fresh computation over drifting state.
    const settled = await getStatusConfig(app);
    expect(settled.applied!.config_hash).toBe(before.applied!.config_hash);
    expect(settled.source.source_hash).toBe(before.source.source_hash);
    expect(configHashValue(await scrape(app))).toBe(before.applied!.config_hash);

    // One more resource, and both surfaces must move together.
    const extra = await seed.createApiKey({
      key_hash: createHash("sha256").update(randomUUID()).digest("hex"),
      allowed_models: ["status-model"],
    });
    let after: StatusConfig | undefined;
    await waitConfigPropagation(async () => {
      after = await getStatusConfig(app!);
      return after.applied?.resource_counts.api_keys === 2;
    });
    expect(after!.applied!.config_hash).not.toBe(before.applied!.config_hash);
    expect(after!.source.source_hash).not.toBe(before.source.source_hash);
    expect(after!.applied!.config_hash).toBe(after!.source.source_hash);
    expect(configHashValue(await scrape(app))).toBe(after!.applied!.config_hash);

    // Take it away again: the digest is over what the gateway holds, so
    // the same configuration has to digest to the same value however many
    // applies and reports happened in between.
    await seed.delete("api_keys", extra.id);
    let restored: StatusConfig | undefined;
    await waitConfigPropagation(async () => {
      restored = await getStatusConfig(app!);
      return restored.applied?.resource_counts.api_keys === 1;
    });
    expect(restored!.source.source_hash).toBe(before.source.source_hash);
    expect(restored!.applied!.config_hash).toBe(before.applied!.config_hash);
    expect(configHashValue(await scrape(app))).toBe(before.applied!.config_hash);
  });

  // The exposition is streamed out in pieces as it is rendered rather than
  // built whole, so a scrape's completeness is now a property of the
  // transport. A body cut between pieces still parses line by line — what
  // gives it away is a family whose header arrives without its samples, or
  // twice.
  test("a streamed scrape arrives whole and well-formed", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await getStatusConfig(app!)).state === "synced");

    const res = await fetch(`${app.metricsUrl}/metrics`);
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/plain; version=0.0.4");
    const text = await res.text();

    expect(text.endsWith("\n")).toBe(true);
    const headers = new Map<string, number>();
    const samples = new Set<string>();
    for (const line of text.split("\n")) {
      if (line === "") continue;
      const typed = line.match(/^# TYPE (\S+) \S+$/);
      if (typed) {
        headers.set(typed[1], (headers.get(typed[1]) ?? 0) + 1);
        continue;
      }
      if (line.startsWith("#")) continue;
      expect(line).toMatch(/^[a-zA-Z_:][a-zA-Z0-9_:]*(\{.*\})? \S+$/);
      samples.add(line.slice(0, line.indexOf(" ")).replace(/\{.*$/, ""));
    }
    expect(headers.size).toBeGreaterThan(0);
    for (const [family, count] of headers) {
      expect(count, `${family} declared ${count} times`).toBe(1);
      const declared = [...samples].some(
        (name) => name === family || name.startsWith(`${family}_`),
      );
      expect(declared, `${family} declared with no samples`).toBe(true);
    }
    // Two scrapes back to back describe the same families: the renderer
    // walks a catalog, and a piece lost between them would show up here
    // as a family present in one and missing from the other.
    const again = await scrape(app);
    const families = (body: string) =>
      body
        .split("\n")
        .filter((line) => line.startsWith("# TYPE "))
        .map((line) => line.split(" ")[2])
        .sort();
    expect(families(again)).toEqual(families(text));
  });

  test("a bad doc alongside good ones degrades without dropping the good config", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const etcd = new EtcdClient();
    // A schema-invalid model written straight to etcd (empty display_name
    // violates minLength) — the CP would reject it, but the DP loader must
    // skip it and keep serving the rest.
    const badId = randomUUID();
    await etcd.put(
      `${app.etcdPrefix}/models/${badId}`,
      JSON.stringify({
        display_name: "",
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: "pk-nonexistent",
      }),
    );

    let cfg: StatusConfig | undefined;
    await waitConfigPropagation(async () => {
      cfg = await getStatusConfig(app!);
      return cfg.state === "degraded" && cfg.rejected.length > 0;
    });

    // The good model is still applied (serving), not dropped.
    expect(cfg!.applied!.resource_counts.models).toBe(1);
    const rej = cfg!.rejected.find((r) => r.resource_id === badId);
    expect(rej, JSON.stringify(cfg!.rejected)).toBeDefined();
    expect(rej!.resource_kind).toBe("models");
    expect(rej!.last_error_kind).toBe("schema_failed");
    expect(rej!.first_seen_at).toMatch(/Z$/);

    // The bad doc surfaces on the metrics listener too.
    const text = await scrape(app);
    expect(text).toMatch(/aisix_config_rejected_resources\{kind="models"\} 1/);
    expect(gaugeValue(text, "aisix_config_last_reload_successful")).toBe(0);

    // End-to-end: a real chat on the still-good model returns 200.
    const proxy = new ProxyClient(app.proxyUrl, CALLER_PLAINTEXT);
    const chat = await proxy.chat({
      model: "status-model",
      messages: [{ role: "user", content: "still serving despite the bad doc" }],
    });
    expect(chat.status, JSON.stringify(chat.body)).toBe(200);

    // Cleanup the bad doc so it doesn't bleed into sibling tests.
    await etcd.delete(`${app.etcdPrefix}/models/${badId}`);
  });

  test("a rejected provider_key never leaks its secret into status or metrics", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const SECRET = "sk-LEAKED-SECRET-abcdef0123456789";
    const leakId = randomUUID();
    const etcd = new EtcdClient();
    // The credential itself is the schema-failing instance: `api_key` must be
    // a string, so an array fails validation and the UNMASKED error message
    // would echo the secret. This asserts the masking actually redacts it —
    // not that the secret merely happens to sit in a valid sibling field.
    await etcd.put(
      `${app.etcdPrefix}/provider_keys/${leakId}`,
      JSON.stringify({ display_name: "leak-pk", api_key: [SECRET] }),
    );

    await waitConfigPropagation(async () => {
      const cfg = await getStatusConfig(app!);
      return cfg.rejected.some((r) => r.resource_id === leakId);
    });

    // The secret must be absent from the whole /status/config body …
    const raw = await fetch(`${app.metricsUrl}/status/config`).then((r) => r.text());
    expect(raw).not.toContain(SECRET);
    // … and from the metrics scrape.
    const text = await scrape(app);
    expect(text).not.toContain(SECRET);

    await etcd.delete(`${app.etcdPrefix}/provider_keys/${leakId}`);
  });

  test("legacy credentialed A2A URLs load with compatibility reporting and no secret leakage", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const USER_SECRET = "a2a-user-secret";
    const PASSWORD_SECRET = "a2a-password-secret";
    const QUERY_SECRET = "a2a-query-secret";
    const agentId = randomUUID();
    const signatureId = randomUUID();
    const cleanId = randomUUID();
    const SIGNATURE_SECRET = "a2a-signature-secret";
    const etcd = new EtcdClient();
    await etcd.put(
      `${app.etcdPrefix}/a2a_agents/${agentId}`,
      JSON.stringify({
        name: "credentialed-agent",
        url: `https://${USER_SECRET}:${PASSWORD_SECRET}@agents.example.com/a2a?access_token=${QUERY_SECRET}`,
      }),
    );
    await etcd.put(
      `${app.etcdPrefix}/a2a_agents/${signatureId}`,
      JSON.stringify({
        name: "signed-agent",
        url: `https://agents.example.com/a2a?X-Amz-Signature=${SIGNATURE_SECRET}`,
      }),
    );
    await etcd.put(
      `${app.etcdPrefix}/a2a_agents/${cleanId}`,
      JSON.stringify({
        name: "clean-agent",
        url: "https://agents.example.com/a2a",
      }),
    );

    // A later authenticated resource is the revision barrier. This keeps the
    // compatibility counters out of propagation readiness and proves the two
    // legacy rows plus the clean negative control have all been observed.
    const barrierCaller = `sk-status-a2a-barrier-${randomUUID()}`;
    const barrier = await seed!.createApiKey({
      key_hash: createHash("sha256").update(barrierCaller).digest("hex"),
      allowed_models: ["status-model"],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${barrierCaller}` },
      });
      if (response.status === 401) {
        await response.text();
        return false;
      }
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      return body.data?.some((model) => model.id === "status-model") === true;
    });
    const cfg = await getStatusConfig(app);

    // Direct etcd writes represent rows from an older control plane. They
    // remain loadable during a rolling upgrade, while the strict write/file
    // validators reject the same URL shapes in their dedicated tests.
    expect(cfg.applied?.resource_counts.a2a_agents).toBe(3);
    expect(
      cfg.partially_compatible.find(
        (entry) =>
          entry.resource_kind === "a2a_agents" && entry.field === "legacy:url_policy",
      )?.count,
    ).toBe(2);
    expect(cfg.rejected.some((entry) => entry.resource_id === agentId)).toBe(false);
    expect(cfg.rejected.some((entry) => entry.resource_id === signatureId)).toBe(false);
    expect(cfg.rejected.some((entry) => entry.resource_id === cleanId)).toBe(false);

    const status = await fetch(`${app.metricsUrl}/status/config`).then((r) => r.text());
    const metrics = await scrape(app);
    for (const secret of [USER_SECRET, PASSWORD_SECRET, QUERY_SECRET, SIGNATURE_SECRET]) {
      expect(status).not.toContain(secret);
      expect(metrics).not.toContain(secret);
    }
    expect(metrics).toMatch(
      /aisix_config_partially_compatible_resources\{kind="a2a_agents"\} 2/,
    );

    await etcd.delete(`${app.etcdPrefix}/a2a_agents/${agentId}`);
    await etcd.delete(`${app.etcdPrefix}/a2a_agents/${signatureId}`);
    await etcd.delete(`${app.etcdPrefix}/a2a_agents/${cleanId}`);
    await seed!.delete("api_keys", barrier.id);
  });

  test("a bad edit to a guardrail in force leaves the rule in force, and says so", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }
    const WORD = "STATUS-LAST-GOOD-WORD";
    const NEW_WORD = "STATUS-REPAIRED-WORD";
    const guardrail = await seed.createGuardrail(
      {
        name: "status-last-good",
        enabled: true,
        hook_point: "input",
        kind: "keyword",
        patterns: [{ kind: "literal", value: WORD }],
      },
      { attach: false },
    );
    const attachment = await seed.attachGuardrailToEnv(guardrail.id);
    const plaintext = `sk-status-last-good-${randomUUID()}`;
    const key = await seed.createApiKey({
      key_hash: createHash("sha256").update(plaintext).digest("hex"),
      allowed_models: ["status-model"],
    });
    const proxy = new ProxyClient(app.proxyUrl, plaintext);
    const statusOf = async (content: string) =>
      (
        await proxy.chat({
          model: "status-model",
          messages: [{ role: "user", content }],
        })
      ).status;
    await waitConfigPropagation(async () => (await statusOf(WORD)) === 422);

    await seed.update("guardrails", guardrail.id, {
      name: "status-last-good",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "regex", value: "oops[(" }],
    });
    let cfg: StatusConfig | undefined;
    await waitConfigPropagation(async () => {
      await statusOf("trigger the rebuild");
      cfg = await getStatusConfig(app!);
      return cfg.rejected.some((r) => r.resource_id === guardrail.id);
    });
    expect(cfg!.state).toBe("degraded");
    const rejection = cfg!.rejected.find((r) => r.resource_id === guardrail.id)!;
    expect(rejection.serving_stale_since).toBeTruthy();
    expect(await statusOf(WORD)).toBe(422);

    await seed.update("guardrails", guardrail.id, {
      name: "status-last-good",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: NEW_WORD }],
    });
    await waitConfigPropagation(async () => (await statusOf(NEW_WORD)) === 422);
    expect(await statusOf(WORD)).toBe(200);
    await waitConfigPropagation(async () => {
      await statusOf("trigger the rebuild");
      cfg = await getStatusConfig(app!);
      return !cfg.rejected.some((r) => r.resource_id === guardrail.id);
    });
    await seed.delete("guardrail_attachments", attachment.id);
    await seed.delete("guardrails", guardrail.id);
    await seed.delete("api_keys", key.id);
  });

  test("a rule that has never run answers by its failure policy, and a monitored one never refuses", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }
    const plaintext = `sk-status-never-built-${randomUUID()}`;
    const key = await seed.createApiKey({
      key_hash: createHash("sha256").update(plaintext).digest("hex"),
      allowed_models: ["status-model"],
    });
    const proxy = new ProxyClient(app.proxyUrl, plaintext);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
    const chat = () =>
      proxy.chat({ model: "status-model", messages: [{ role: "user", content: "hello" }] });
    expect((await chat()).status).toBe(200);

    const broken = (name: string, extra: Record<string, unknown>) =>
      seed!.createGuardrail(
        {
          name,
          enabled: true,
          hook_point: "input",
          kind: "keyword",
          patterns: [{ kind: "regex", value: "oops[(" }],
          ...extra,
        },
        { attach: false },
      );
    const bypasses = async () =>
      (await scrape(app!))
        .split("\n")
        .filter((l) => l.startsWith("aisix_guardrail_bypasses_total{"))
        .reduce((sum, l) => sum + Number(l.slice(l.lastIndexOf(" ") + 1)), 0);

    // Fail-closed: refused, and the caller is told the rule could not run.
    const closed = await broken("status-never-built-closed", { fail_open: false });
    const closedAttachment = await seed.attachGuardrailToEnv(closed.id);
    await waitConfigPropagation(async () => (await chat()).status === 422);
    const refused = await chat();
    const body = refused.body as { error?: { code?: string } };
    expect(refused.status).toBe(422);
    expect(body.error?.code).toBe("guardrail_unavailable");
    await seed.delete("guardrail_attachments", closedAttachment.id);
    await seed.delete("guardrails", closed.id);
    await waitConfigPropagation(async () => (await chat()).status === 200);

    // Fail-open: goes through, and the bypass is on the record.
    const open = await broken("status-never-built-open", { fail_open: true });
    const openAttachment = await seed.attachGuardrailToEnv(open.id);
    const before = await bypasses();
    await waitConfigPropagation(async () => {
      expect((await chat()).status).toBe(200);
      return (await bypasses()) > before;
    });
    await seed.delete("guardrail_attachments", openAttachment.id);
    await seed.delete("guardrails", open.id);

    // Monitored, on a fail-closed row: observes, never refuses.
    const monitored = await broken("status-never-built-monitored", {
      fail_open: false,
      enforcement_mode: "monitor",
    });
    const monitoredAttachment = await seed.attachGuardrailToEnv(monitored.id);
    await waitConfigPropagation(async () => {
      expect((await chat()).status).toBe(200);
      const cfg = await getStatusConfig(app!);
      return cfg.rejected.some((r) => r.resource_id === monitored.id);
    });
    expect((await chat()).status).toBe(200);
    await seed.delete("guardrail_attachments", monitoredAttachment.id);
    await seed.delete("guardrails", monitored.id);
    await seed.delete("api_keys", key.id);
  });

  test("a guardrail that fails to build degrades status and clears after repair", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    const guardrail = await seed.createGuardrail(
      {
        name: "status-broken-script",
        enabled: true,
        hook_point: "input",
        kind: "custom",
        script: "export function checkInput( {",
      },
      { attach: false },
    );
    const attachment = await seed.attachGuardrailToEnv(guardrail.id);

    const buildProbePlaintext = `sk-status-build-${randomUUID()}`;
    const buildProbe = await seed.createApiKey({
      key_hash: createHash("sha256").update(buildProbePlaintext).digest("hex"),
      allowed_models: ["status-model"],
    });
    const proxy = new ProxyClient(app.proxyUrl, buildProbePlaintext);
    await waitConfigPropagation(async () => {
      return (await proxy.listModels()).status === 200;
    });

    // The index build after the snapshot swap, not the loader, is what
    // discovers the script the loader deliberately accepted cannot compile.
    // A rule that cannot run, under a fail-closed policy, refuses.
    expect(
      (
        await proxy.chat({
          model: "status-model",
          messages: [{ role: "user", content: "trigger the lazy build" }],
        })
      ).status,
    ).toBe(422);

    let cfg: StatusConfig | undefined;
    await waitConfigPropagation(async () => {
      cfg = await getStatusConfig(app!);
      return cfg.rejected.some((r) => r.resource_id === guardrail.id);
    });
    const rejection = cfg!.rejected.find((r) => r.resource_id === guardrail.id)!;
    expect(cfg!.state).toBe("degraded");
    expect(rejection.resource_kind).toBe("guardrails");
    expect(rejection.last_error_kind).toBe("schema_failed");
    expect(rejection.last_error).toBe(
      "guardrail runtime build failed: compile_failed at config.script",
    );
    const serializedStatus = JSON.stringify(cfg);
    expect(serializedStatus).not.toContain("status-broken-script");
    expect(serializedStatus).not.toContain("export function checkInput");

    const text = await scrape(app);
    expect(text).toMatch(/aisix_config_rejected_resources\{kind="guardrails"\} 1/);
    expect(configHashValue(text)).toBe(cfg!.applied!.config_hash);
    expect(configHashValue(text)).not.toBe(cfg!.source.source_hash);

    await seed.update("guardrails", guardrail.id, {
      name: "status-fixed-guardrail",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: "NEVER-MATCH" }],
    });
    const repairProbePlaintext = `sk-status-repair-${randomUUID()}`;
    const repairProbe = await seed.createApiKey({
      key_hash: createHash("sha256").update(repairProbePlaintext).digest("hex"),
      allowed_models: ["status-model"],
    });
    const repairProxy = new ProxyClient(app.proxyUrl, repairProbePlaintext);
    await waitConfigPropagation(async () => {
      return (await repairProxy.listModels()).status === 200;
    });
    expect(
      (
        await repairProxy.chat({
          model: "status-model",
          messages: [{ role: "user", content: "trigger the repaired build" }],
        })
      ).status,
    ).toBe(200);
    await waitConfigPropagation(async () => {
      cfg = await getStatusConfig(app!);
      return !cfg.rejected.some((r) => r.resource_id === guardrail.id);
    });
    expect(cfg!.state).toBe("synced");
    const repairedMetrics = await scrape(app);
    expect(configHashValue(repairedMetrics)).toBe(cfg!.applied!.config_hash);
    expect(configHashValue(repairedMetrics)).toBe(cfg!.source.source_hash);

    await seed.delete("guardrail_attachments", attachment.id);
    await seed.delete("guardrails", guardrail.id);
    await seed.delete("api_keys", buildProbe.id);
    await seed.delete("api_keys", repairProbe.id);
  });
});

describe("status/config: standalone file source", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    app = await spawnApp({
      resourcesFile: `
_format_version: "1"
provider_keys:
  - display_name: file-status-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstream.baseUrl}/v1
models:
  - display_name: file-status-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: file-status-pk
api_keys:
  - display_name: file-status-caller
    key_hash: ${CALLER_KEY_HASH}
    allowed_models: ["file-status-model"]
`,
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("file mode boots synced with no revision fields", async () => {
    if (!app) throw new Error("setup failed");
    // File mode is synchronous at boot — no propagation wait needed.
    const cfg = await getStatusConfig(app);
    expect(cfg.state).toBe("synced");
    expect(cfg.source.type).toBe("file");
    // Revision + connected fields are etcd-only and must be absent in file mode.
    expect(cfg.source.connected).toBeUndefined();
    expect(cfg.source.observed_revision).toBeUndefined();
    expect(cfg.applied?.applied_revision).toBeUndefined();
    expect(cfg.source.source_hash).toMatch(/^[0-9a-f]{64}$/);
    expect(cfg.applied!.resource_counts.models).toBe(1);
    expect(cfg.applied!.resource_counts.provider_keys).toBe(1);
    expect(cfg.rejected).toHaveLength(0);

    // The scrape omits the etcd-only revision/connected series in file mode.
    const text = await scrape(app);
    expect(gaugeValue(text, "aisix_config_last_reload_successful")).toBe(1);
    expect(text).not.toContain("aisix_config_observed_revision");
    expect(text).not.toContain("aisix_config_source_connected");
  });

  test("status/ready is 200 once the file config is applied", async () => {
    if (!app) throw new Error("setup failed");
    const res = await fetch(`${app.metricsUrl}/status/ready`);
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("ok");
  });
});

// A DP pointed at an unreachable etcd never applies a config, so it stays in
// `never_loaded` — but that also makes `/admin/v1/health` (an etcd-backed
// store read) fail, which the standard harness gate waits on. So this case
// spawns the binary directly and gates only on the metrics/status listener,
// which binds independently of the config source.
interface MinimalApp {
  proxyUrl: string;
  metricsUrl: string;
  stop(): Promise<void>;
}

async function spawnPointedAtDeadEtcd(): Promise<MinimalApp> {
  const [proxyPort, adminPort, metricsPort, deadPort] = await pickFreePorts(4);
  const dir = await mkdtemp(join(tmpdir(), "aisix-status-nl-"));
  const cfg = {
    // etcd-client's connect is lazy (no eager dial without auth), so the
    // gateway boots and binds its metrics listener even though nothing answers
    // here; the watch supervisor's first load never succeeds → never_loaded.
    // The proxy listener stays closed for exactly that reason, which is why
    // this case gates on the metrics listener alone.
    etcd: {
      endpoints: [`http://127.0.0.1:${deadPort}`],
      prefix: "/aisix-never-loaded",
    },
    proxy: {
      addr: `127.0.0.1:${proxyPort}`,
      request_body_limit_bytes: 10485760,
      // This file spawns the binary itself, so it has to honour the
      // suite-wide serving mode the shared harness applies.
      ...(suiteThreadPerCore !== undefined ? { thread_per_core: suiteThreadPerCore } : {}),
    },
    admin: { addr: `127.0.0.1:${adminPort}`, admin_keys: [`admin-${randomUUID()}`] },
    observability: {
      service_name: "aisix-status-nl",
      log_level: "warn",
      metrics: {
        prometheus: { enabled: true, path: "/metrics", addr: `127.0.0.1:${metricsPort}` },
      },
    },
    cache: { backend: "memory" },
    // Nothing balances this gateway, so the exit need not wait out a window.
    shutdown: { min_drain_secs: 0 },
  };
  const cfgPath = join(dir, "config.yaml");
  await writeFile(cfgPath, yamlStringify(cfg), "utf8");

  // Strip AISIX_* so the harness's own env can't override the config.
  const childEnv: Record<string, string> = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v !== undefined && !k.startsWith("AISIX_")) childEnv[k] = v;
  }
  childEnv.RUST_LOG = "warn";

  const child: ChildProcess = spawn(BIN_PATH, ["--config", cfgPath], {
    stdio: ["ignore", "ignore", "ignore"],
    env: childEnv,
  });
  recordProfileOwner(child.pid);
  const metricsUrl = `http://127.0.0.1:${metricsPort}`;

  // Gate only on the metrics/status listener answering (any status — a 503
  // from /status/ready still means the listener is up).
  const deadline = Date.now() + 10_000;
  for (;;) {
    if (child.exitCode !== null) throw new Error(`aisix exited early code=${child.exitCode}`);
    try {
      const res = await fetch(`${metricsUrl}/status/ready`);
      await res.text();
      break;
    } catch {
      if (Date.now() > deadline) throw new Error("metrics listener never came up");
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  return {
    proxyUrl: `http://127.0.0.1:${proxyPort}`,
    metricsUrl,
    async stop() {
      // SIGTERM and the gateway's own exit, so its coverage profile is written whole.
      await stopGateway(child);
      await rm(dir, { recursive: true, force: true });
    },
  };
}

describe("status/ready: never_loaded before any config", () => {
  let app: MinimalApp | undefined;

  beforeAll(async () => {
    app = await spawnPointedAtDeadEtcd();
  });

  afterAll(async () => {
    await app?.stop();
  });

  test("status/ready is 503 and status/config reports never_loaded", async () => {
    if (!app) throw new Error("setup failed");

    const ready = await fetch(`${app.metricsUrl}/status/ready`);
    expect(ready.status).toBe(503);
    expect(await ready.text()).toBe("no configuration available");

    const res = await fetch(`${app.metricsUrl}/status/config`);
    expect(res.status).toBe(200);
    const cfg = (await res.json()) as StatusConfig;
    expect(cfg.state).toBe("never_loaded");
    expect(cfg.applied).toBeUndefined();
    // The etcd source shows disconnected while it keeps retrying.
    expect(cfg.source.type).toBe("etcd");
    expect(cfg.source.connected).toBe(false);
  });
});
