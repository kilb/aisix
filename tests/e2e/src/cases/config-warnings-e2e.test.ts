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

// E2E: configuration that loads and serves but does not do what it reads as
// doing is called out on `GET /status/config`.
//
// Two shapes changed meaning between releases, deliberately, and a row written
// for the older meaning loads without complaint:
//
// - a `cooldown` block that does not say `enabled: true` no longer takes the
//   model out of rotation, whatever else the block tunes;
// - a guardrail with no attachment no longer inspects anything.
//
// Neither is a failure to load, so `state` stays `synced`. An operator who
// reads the status still has to be told that a rule they believe is in force
// is not.

const CALLER_PLAINTEXT = "sk-config-warnings-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const CALLER_KEY_ENV = "CONFIG_WARNINGS_CALLER_KEY";

interface ConfigWarning {
  code: string;
  resource_kind: string;
  count: number;
  resource_ids: string[];
  message: string;
}

interface StatusConfig {
  state: string;
  warnings: ConfigWarning[];
}

async function statusConfig(app: SpawnedApp): Promise<StatusConfig> {
  const res = await fetch(`${app.metricsUrl}/status/config`);
  expect(res.status).toBe(200);
  return (await res.json()) as StatusConfig;
}

describe("status warnings: etcd source", () => {
  let app: SpawnedApp | undefined;
  let seed: SeedClient | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;
  let providerKeyId = "";

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream();
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "warnings-pk",
      provider: "openai",
      api_key: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    providerKeyId = pk.id;
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["*"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("names both shapes, and clears when the row is corrected", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }
    const clean = await statusConfig(app);
    expect(clean.warnings).toEqual([]);

    const inert = await seed.createModel({
      display_name: "warnings-inert-cooldown",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: providerKeyId,
      cooldown: { default_seconds: 60 },
    });
    await seed.createModel({
      display_name: "warnings-explicitly-off",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: providerKeyId,
      cooldown: { enabled: false, default_seconds: 60 },
    });
    await seed.createModel({
      display_name: "warnings-on",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: providerKeyId,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    const unattached = await seed.createGuardrail(
      {
        name: "warnings-unattached",
        kind: "keyword",
        patterns: [{ kind: "literal", value: "warnings-phrase" }],
      },
      { attach: false },
    );
    await seed.createGuardrail({
      name: "warnings-attached",
      kind: "keyword",
      patterns: [{ kind: "literal", value: "warnings-other-phrase" }],
    });

    await waitConfigPropagation(
      async () => (await statusConfig(app!)).warnings.length === 2,
    );
    const cfg = await statusConfig(app);
    // Serving as configured: these rows did not fail to load.
    expect(cfg.state).toBe("synced");

    const cooldown = cfg.warnings.find((w) => w.resource_kind === "models");
    expect(cooldown?.count).toBe(1);
    expect(cooldown?.resource_ids).toEqual([inert.id]);
    expect(cooldown?.message).toContain("enabled");

    const guardrail = cfg.warnings.find((w) => w.resource_kind === "guardrails");
    expect(guardrail?.count).toBe(1);
    expect(guardrail?.resource_ids).toEqual([unattached.id]);
    expect(guardrail?.message).toContain("attachment");

    await seed.update("models", inert.id, {
      display_name: "warnings-inert-cooldown",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: providerKeyId,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    await waitConfigPropagation(async () => {
      const warnings = (await statusConfig(app!)).warnings;
      return warnings.length === 1 && warnings[0]?.resource_kind === "guardrails";
    });
  });
});

describe("status warnings: file source", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    app = await spawnApp({
      resourcesFile: `
_format_version: "1"
provider_keys:
  - display_name: warnings-file-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstream.baseUrl}/v1
models:
  - display_name: warnings-file-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: warnings-file-pk
    cooldown:
      max_seconds: 120
api_keys:
  - display_name: warnings-file-caller
    key_env: ${CALLER_KEY_ENV}
    allowed_models: ["warnings-file-model"]
guardrails:
  - name: warnings-file-unattached
    kind: keyword
    patterns:
      - kind: literal
        value: warnings-file-phrase
`,
      extraEnv: { [CALLER_KEY_ENV]: CALLER_PLAINTEXT },
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a file carrying both shapes is told about both", async () => {
    if (!app) throw new Error("setup failed");
    const cfg = await statusConfig(app);
    expect(cfg.state).toBe("synced");
    expect(cfg.warnings.map((w) => w.resource_kind).sort()).toEqual(["guardrails", "models"]);
  });
});
