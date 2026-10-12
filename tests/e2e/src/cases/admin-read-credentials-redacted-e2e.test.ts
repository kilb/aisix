import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  AdminClient,
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: the Admin API never reads back a stored credential.
//
// User journey: an operator (or the console built on top of the Admin API)
// lists and inspects the gateway's configuration. Resources that hold live
// upstream credentials — a provider key's `api_key` and the string values of
// its `request.default_headers` / `request.default_body_fields`, a
// guardrail's provider key or AWS secret, an MCP server's or A2A agent's
// `secret`, an OTLP exporter's `headers` — come back with every credential
// replaced by the fixed placeholder `[REDACTED]`, on the list and the
// get-by-id route alike. Everything that identifies or configures the
// resource is returned as stored, and the gateway itself keeps
// authenticating upstream with the real value: redaction is a property of
// the read response, not of the stored configuration. This is the same set
// of fields `aisix export` replaces with placeholders.

const REDACTED = "[REDACTED]";
const CALLER = "sk-admin-redaction-caller";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");

const SECRET = {
  providerKey: "sk-live-PROVIDER-MARKER-1",
  providerHeader: "tok-HEADER-MARKER-2",
  providerBody: "tok-BODY-MARKER-3",
  providerNested: "tok-NESTED-MARKER-9",
  legacyProviderKey: "sk-LEGACY-MARKER-10",
  aliyunSecret: "aliyun-SECRET-MARKER-11",
  guardrailKey: "sk-GUARDRAIL-MARKER-4",
  guardrailAws: "aws-SECRET-MARKER-5",
  mcp: "mcp-SECRET-MARKER-6",
  a2a: "a2a-SECRET-MARKER-7",
  otlp: "hcaik-OTLP-MARKER-8",
};

interface Entry {
  id: string;
  value: Record<string, unknown>;
}

describe("Admin API reads return credentials as [REDACTED]", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let admin: AdminClient;
  const ids: Record<string, string> = {};
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream();
    // The Admin API read surface is the subject here, so bind its listener.
    app = await spawnApp({ admin: true });
    admin = new AdminClient(app.adminUrl, app.adminKey);
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "creds-pk",
      api_key: SECRET.providerKey,
      api_base: `${upstream.baseUrl}/v1`,
      request: {
        default_headers: { "x-tenant-token": SECRET.providerHeader },
        default_body_fields: {
          partner_token: SECRET.providerBody,
          safe_prompt: true,
          extra: { nested_token: SECRET.providerNested },
        },
      },
    });
    ids.provider_keys = pk.id;
    // Stored under the field's former name, `secret`.
    ids.legacy = (
      await seed.createProviderKey({
        display_name: "creds-legacy-pk",
        secret: SECRET.legacyProviderKey,
        api_base: "https://legacy.example.com/v1",
      })
    ).id;
    await seed.createModel({
      display_name: "creds-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // Disabled rows: the Admin API lists them, and they stay out of traffic.
    ids.guardrails = (
      await seed.createGuardrail({
        name: "creds-moderation",
        enabled: false,
        kind: "openai_moderation",
        api_key: SECRET.guardrailKey,
      })
    ).id;
    ids.bedrock = (
      await seed.createGuardrail({
        name: "creds-bedrock",
        enabled: false,
        kind: "bedrock",
        guardrail_id: "credsgr00001",
        guardrail_version: "DRAFT",
        region: "us-east-1",
        aws_credentials: { kind: "static", access_key_id: "AKIDCREDS00001", secret_access_key: SECRET.guardrailAws },
        latency_mode: { kind: "serial" },
      })
    ).id;
    ids.aliyun = (
      await seed.createGuardrail({
        name: "creds-aliyun",
        enabled: false,
        kind: "aliyun_text_moderation",
        region: "cn-shanghai",
        access_key_id: "LTAICREDS00001",
        access_key_secret: SECRET.aliyunSecret,
      })
    ).id;
    ids.mcp_servers = randomUUID();
    await seed.update("mcp_servers", ids.mcp_servers, {
      name: "creds-mcp",
      url: "https://mcp.example.com/mcp",
      auth_type: "bearer",
      secret: SECRET.mcp,
      enabled: false,
    });
    ids.a2a_agents = randomUUID();
    await seed.update("a2a_agents", ids.a2a_agents, {
      name: "creds-agent",
      url: "https://agent.example.com/a2a",
      protocol_version: "0.3",
      auth_type: "bearer",
      secret: SECRET.a2a,
      enabled: false,
    });
    ids.observability_exporters = (
      await seed.createObservabilityExporter({
        name: "creds-otlp",
        enabled: false,
        kind: "otlp_http",
        endpoint: "https://otlp.example.com/v1/traces",
        headers: { "x-honeycomb-team": SECRET.otlp },
      })
    ).id;
    // Caller key last: once it authenticates, every resource above is in
    // the snapshot.
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["creds-model"] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  /** The list entry and the get-by-id entry for `id`, after checking neither leaks a credential. */
  async function read(kind: string, id: string): Promise<Entry[]> {
    const list = await admin.json<Entry[]>("GET", `/admin/v1/${kind}`);
    const one = await admin.json<Entry>("GET", `/admin/v1/${kind}/${id}`);
    for (const body of [list, one]) {
      const text = JSON.stringify(body);
      for (const secret of Object.values(SECRET)) {
        expect(text, `${kind} leaks a credential`).not.toContain(secret);
      }
    }
    const listed = list.find((e) => e.id === id);
    expect(listed, `${kind} list carries ${id}`).toBeDefined();
    return [listed!, one];
  }

  test("provider_keys: api_key and request header/body values", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const { value } of await read("provider_keys", ids.provider_keys!)) {
      expect(value.api_key).toBe(REDACTED);
      expect(value.display_name).toBe("creds-pk");
      expect(value.api_base).toBe(`${upstream!.baseUrl}/v1`);
      const request = value.request as Record<string, Record<string, unknown>>;
      expect(request.default_headers).toEqual({ "x-tenant-token": REDACTED });
      expect(request.default_body_fields).toEqual({
        partner_token: REDACTED,
        safe_prompt: true,
        extra: { nested_token: REDACTED },
      });
    }
    for (const { value } of await read("provider_keys", ids.legacy!)) {
      expect(value.api_key).toBe(REDACTED);
      expect(value.api_base).toBe("https://legacy.example.com/v1");
    }
  });

  test("guardrails: provider api_key and the AWS secret", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const { value } of await read("guardrails", ids.guardrails!)) {
      expect(value.api_key).toBe(REDACTED);
      expect(value.name).toBe("creds-moderation");
    }
    for (const { value } of await read("guardrails", ids.bedrock!)) {
      expect(value.aws_credentials).toEqual({
        kind: "static",
        access_key_id: "AKIDCREDS00001",
        secret_access_key: REDACTED,
      });
      expect(value.guardrail_id).toBe("credsgr00001");
    }
    for (const { value } of await read("guardrails", ids.aliyun!)) {
      expect(value.access_key_secret).toBe(REDACTED);
      expect(value.access_key_id).toBe("LTAICREDS00001");
    }
  });

  test("mcp_servers and a2a_agents: secret", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const { value } of await read("mcp_servers", ids.mcp_servers!)) {
      expect(value.secret).toBe(REDACTED);
      expect(value.url).toBe("https://mcp.example.com/mcp");
    }
    for (const { value } of await read("a2a_agents", ids.a2a_agents!)) {
      expect(value.secret).toBe(REDACTED);
      expect(value.url).toBe("https://agent.example.com/a2a");
    }
  });

  test("observability_exporters: header values", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const { value } of await read("observability_exporters", ids.observability_exporters!)) {
      expect(value.headers).toEqual({ "x-honeycomb-team": REDACTED });
      expect(value.endpoint).toBe("https://otlp.example.com/v1/traces");
    }
  });

  test("the gateway still authenticates upstream with the stored credential", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const res = await new ProxyClient(app.proxyUrl, CALLER).chat({
      model: "creds-model",
      messages: [{ role: "user", content: "hello" }],
    });
    expect(res.status).toBe(200);
    const sent = upstream.receivedRequests.at(-1)!;
    expect(sent.headers.authorization).toBe(`Bearer ${SECRET.providerKey}`);
    expect(sent.headers["x-tenant-token"]).toBe(SECRET.providerHeader);
    const body = JSON.parse(sent.body) as { partner_token: string; extra: { nested_token: string } };
    expect(body.partner_token).toBe(SECRET.providerBody);
    expect(body.extra.nested_token).toBe(SECRET.providerNested);
  });
});

// File mode: the Admin API serves the very configuration the gateway runs
// on, with `${VAR}` references already resolved. A read must redact the
// resolved value and leave the running configuration untouched.
describe("Admin API reads redact credentials in file mode too", () => {
  const FILE_SECRET = "sk-FILE-ENV-MARKER-12";
  const FILE_CALLER = "sk-admin-redaction-file-caller";
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    app = await spawnApp({
      admin: true,
      extraEnv: { CREDS_E2E_PROVIDER_KEY: FILE_SECRET, CREDS_E2E_CALLER_KEY: FILE_CALLER },
      resourcesFile: `
_format_version: "1"
provider_keys:
  - display_name: creds-file-pk
    provider: openai
    api_key: \${CREDS_E2E_PROVIDER_KEY}
    api_base: ${upstream.baseUrl}/v1
models:
  - display_name: creds-file-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: creds-file-pk
api_keys:
  - display_name: creds-file-caller
    key_env: CREDS_E2E_CALLER_KEY
    allowed_models: ["creds-file-model"]
`,
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("the resolved value is redacted and the gateway keeps using it", async () => {
    if (!app || !upstream) throw new Error("setup failed");
    const admin = new AdminClient(app.adminUrl, app.adminKey);
    const chat = () =>
      new ProxyClient(app!.proxyUrl, FILE_CALLER).chat({
        model: "creds-file-model",
        messages: [{ role: "user", content: "hello" }],
      });

    for (let round = 0; round < 2; round++) {
      const list = await admin.json<Entry[]>("GET", "/admin/v1/provider_keys");
      expect(JSON.stringify(list)).not.toContain(FILE_SECRET);
      const entry = list.find((e) => e.value.display_name === "creds-file-pk")!;
      expect(entry.value.api_key).toBe(REDACTED);
      const one = await admin.json<Entry>("GET", `/admin/v1/provider_keys/${entry.id}`);
      expect(one.value.api_key).toBe(REDACTED);

      // Reading did not change what the gateway sends upstream.
      expect((await chat()).status).toBe(200);
      expect(upstream.receivedRequests.at(-1)!.headers.authorization).toBe(`Bearer ${FILE_SECRET}`);
    }
  });
});
