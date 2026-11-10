import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  etcdEndpoint,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMcpUpstream,
  startOpenAiUpstream,
  waitConfigPropagation,
  type McpUpstream,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: the `aisix export` round-trip — the migration path that closes the
// loop etcd → export → file → same behavior.
//
// A gateway is populated via the seed front door (canonical documents
// written straight to etcd, as the control plane does in managed mode).
// `aisix export` reads that etcd store and emits a resources.yaml; a
// second, file-mode gateway loads the exported file and must be
// observably identical to the etcd-backed one — same /v1/models listing,
// same chat completion, same allowed_models 403. The exported file also
// must carry no live credential at default settings; the provider
// secret is replaced with a ${VAR} placeholder the file-mode gateway
// interpolates from its environment.
//
// Reference: OpenAI Chat Completions API spec
// (https://platform.openai.com/docs/api-reference/chat/create) for the
// proxy surface the case drives.

const execFileP = promisify(execFile);

const BIN_PATH =
  process.env.AISIX_BIN ?? join(process.cwd(), "..", "..", "target", "debug", "aisix");
const ETCD_ENDPOINT = etcdEndpoint();

const CALLER_PLAINTEXT = "sk-export-roundtrip-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
// A distinctive marker so the no-leak assertion is unambiguous.
const PROVIDER_SECRET = "sk-provider-secret-must-not-leak-1a2b3c";

/** Extract every `${VAR}` interpolation reference the export emitted. */
function placeholderVars(yaml: string): string[] {
  return [...yaml.matchAll(/\$\{([^}]+)\}/g)].map((m) => m[1]!);
}

describe("aisix export: etcd → export → file round-trip", () => {
  let etcdApp: SpawnedApp | undefined;
  let fileApp: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) {
      // Follow the harness convention: defer to each test's ctx.skip()
      // rather than throwing when etcd is unavailable.
      return;
    }

    upstream = await startOpenAiUpstream();

    // Populate the source-of-truth gateway's etcd exactly as the control
    // plane would: one provider key (with a real secret), an allowed
    // model, a forbidden model, and a caller key scoped to the allowed
    // model only.
    etcdApp = await spawnApp();
    const seed = new SeedClient(etcd, etcdApp.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "rt-pk",
      api_key: PROVIDER_SECRET,
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "rt-allowed",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createModel({
      display_name: "rt-forbidden",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["rt-allowed"],
    });

    // etcd propagates asynchronously — gate on the allowed model serving
    // before the test reads etcd via export.
    const probe = new ProxyClient(etcdApp.proxyUrl, CALLER_PLAINTEXT);
    await waitConfigPropagation(async () => {
      const r = await probe.chat({
        model: "rt-allowed",
        messages: [{ role: "user", content: "ready-probe" }],
      });
      return r.status === 200;
    });
  });

  afterAll(async () => {
    await fileApp?.exit();
    await etcdApp?.exit();
    await upstream?.close();
  });

  test("exported file has no live secret and reproduces the gateway's behavior", async (ctx) => {
    if (!etcdReachable || !etcdApp || !upstream) {
      ctx.skip();
      return;
    }

    // 1) Run `aisix export` against the seeded etcd store.
    const { stdout: yaml, stderr } = await execFileP(BIN_PATH, [
      "export",
      "--etcd",
      ETCD_ENDPOINT,
      "--prefix",
      etcdApp.etcdPrefix,
    ]);

    // 2) No live credential at default settings — the provider secret is
    // replaced with a placeholder, and the companion list on stderr tells
    // the operator which variable to set.
    expect(yaml).not.toContain(PROVIDER_SECRET);
    // Diagnostics on stderr must not leak the secret into CI logs either.
    expect(stderr).not.toContain(PROVIDER_SECRET);
    expect(yaml).toContain("_format_version");
    const vars = placeholderVars(yaml);
    expect(vars.length).toBeGreaterThan(0);
    expect(stderr).toContain("rt-pk");
    for (const v of vars) {
      expect(stderr).toContain(v);
    }

    // 3) Load the exported file into a file-mode gateway, supplying the
    // real secret through the placeholder variables the export emitted.
    const extraEnv = Object.fromEntries(vars.map((v) => [v, PROVIDER_SECRET]));
    fileApp = await spawnApp({ resourcesFile: yaml, extraEnv });

    const fileProxy = new ProxyClient(fileApp.proxyUrl, CALLER_PLAINTEXT);
    const etcdProxy = new ProxyClient(etcdApp.proxyUrl, CALLER_PLAINTEXT);

    // 4a) /v1/models — same listing (modulo the per-response timestamp).
    const fileModels = await fileProxy.listModels();
    const etcdModels = await etcdProxy.listModels();
    expect(fileModels.status).toBe(200);
    expect(etcdModels.status).toBe(200);
    const normalize = (body: unknown) => {
      const b = body as { object: string; data: Array<Record<string, unknown>> };
      return {
        object: b.object,
        data: b.data
          .map(({ id, object, owned_by }) => ({ id, object, owned_by }))
          .sort((a, z) => String(a.id).localeCompare(String(z.id))),
      };
    };
    expect(normalize(fileModels.body)).toEqual(normalize(etcdModels.body));

    // 4b) Chat on the allowed model — 200 with the same completion body
    // from the shared mock upstream. Proves the ${VAR}-interpolated
    // provider credential reached the upstream on the file side.
    const fileChat = await fileProxy.chat({
      model: "rt-allowed",
      messages: [{ role: "user", content: "round-trip" }],
    });
    const etcdChat = await etcdProxy.chat({
      model: "rt-allowed",
      messages: [{ role: "user", content: "round-trip" }],
    });
    expect(fileChat.status).toBe(200);
    expect(etcdChat.status).toBe(200);
    const message = (r: unknown) =>
      (r as { choices: Array<{ message: { role: string; content: string } }> }).choices[0]
        ?.message;
    expect(message(fileChat.body)).toEqual(message(etcdChat.body));

    // 4c) allowed_models enforcement — same 403 envelope on the model the
    // caller key does not grant.
    const fileBlocked = await fileProxy.chat({
      model: "rt-forbidden",
      messages: [{ role: "user", content: "blocked" }],
    });
    const etcdBlocked = await etcdProxy.chat({
      model: "rt-forbidden",
      messages: [{ role: "user", content: "blocked" }],
    });
    expect(fileBlocked.status).toBe(403);
    expect(etcdBlocked.status).toBe(403);
    const envelope = (r: unknown) => {
      const e = (r as { error: { type?: string; code?: unknown } }).error;
      return { type: e.type, code: e.code };
    };
    expect(envelope(fileBlocked.body)).toEqual(envelope(etcdBlocked.body));
  });

  test("a passthrough route survives the move, with a guardrail scoped to it", async (ctx) => {
    if (!etcdReachable || !upstream) {
      ctx.skip();
      return;
    }
    const etcd = new EtcdClient();
    const prefix = `/aisix-export-route-${randomUUID()}`;
    const seed = new SeedClient(etcd, prefix);
    const routeCaller = "sk-export-route-caller";
    let routeApp: SpawnedApp | undefined;
    try {
      const pk = await seed.createProviderKey({
        display_name: "route-pk",
        provider: "openai",
        api_key: "sk-route-secret",
      });
      const route = await seed.createPassthroughRoute({
        name: "exported-route",
        path_prefix: "/passthrough/exported",
        target_url: `${upstream.baseUrl}/v1`,
        provider_key_id: pk.id,
      });
      const guardrail = await seed.createGuardrail(
        {
          name: "route-rule",
          kind: "keyword",
          patterns: [{ kind: "literal", value: "route-forbidden-phrase" }],
        },
        { attach: false },
      );
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: guardrail.id,
        scope_type: "passthrough_route",
        scope_id: route.id,
        priority: 100,
      });
      await seed.createApiKey({
        key_hash: createHash("sha256").update(routeCaller).digest("hex"),
        allowed_routes: ["exported-route"],
      });

      const { stdout: yaml } = await execFileP(BIN_PATH, [
        "export",
        "--etcd",
        ETCD_ENDPOINT,
        "--prefix",
        prefix,
      ]);
      const vars = placeholderVars(yaml);
      routeApp = await spawnApp({
        resourcesFile: yaml,
        extraEnv: Object.fromEntries(vars.map((v) => [v, "sk-route-secret"])),
      });

      const call = (content: string) =>
        fetch(`${routeApp!.proxyUrl}/passthrough/exported/chat/completions`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${routeCaller}`,
            "content-type": "application/json",
          },
          body: JSON.stringify({
            model: "gpt-4o-mini",
            messages: [{ role: "user", content }],
          }),
        });
      const allowed = await call("hello");
      await allowed.arrayBuffer();
      expect(allowed.status).toBe(200);
      const blocked = await call("contains route-forbidden-phrase here");
      await blocked.arrayBuffer();
      expect(blocked.status).toBeGreaterThanOrEqual(400);
    } finally {
      await routeApp?.exit();
      await etcd.deletePrefix(prefix);
    }
  });

  // A password in a URL is sent as Basic auth, so it is a credential like a
  // provider key's `api_key`: the export writes it as a placeholder and keeps
  // the rest of the URL, the file loads once the variable holds it, and the
  // Admin API shows it redacted.
  test("a URL password is exported as a placeholder and read back redacted", async (ctx) => {
    if (!etcdReachable || !upstream) {
      ctx.skip();
      return;
    }
    const etcd = new EtcdClient();
    const prefix = `/aisix-export-url-${randomUUID()}`;
    const seed = new SeedClient(etcd, prefix);
    const password = "url-password-must-not-leak-9z8y";
    const targetUrl = upstream.baseUrl.replace("http://", `http://svc:${password}@`) + "/v1";
    let fileApp: SpawnedApp | undefined;
    try {
      const pk = await seed.createProviderKey({
        display_name: "url-pk",
        provider: "openai",
        api_key: "sk-url-secret",
        api_base: targetUrl,
      });
      await seed.createPassthroughRoute({
        name: "url-route",
        path_prefix: "/passthrough/url",
        target_url: targetUrl,
        provider_key_id: pk.id,
      });

      const { stdout: yaml } = await execFileP(BIN_PATH, [
        "export",
        "--etcd",
        ETCD_ENDPOINT,
        "--prefix",
        prefix,
      ]);
      expect(yaml).not.toContain(password);
      const vars = placeholderVars(yaml);
      const urlVars = vars.filter((v) => v.endsWith("_PASSWORD"));
      expect(urlVars).toHaveLength(2);
      expect(yaml).toContain("http://svc:${");

      fileApp = await spawnApp({
        admin: true,
        resourcesFile: yaml,
        extraEnv: Object.fromEntries(
          vars.map((v) => [v, urlVars.includes(v) ? password : "sk-url-secret"]),
        ),
      });
      const read = async (path: string) => {
        const res = await fetch(`${fileApp!.adminUrl}${path}`, {
          headers: { authorization: `Bearer ${fileApp!.adminKey}` },
        });
        expect(res.status).toBe(200);
        return res.text();
      };
      for (const path of ["/admin/v1/provider_keys", "/admin/v1/passthrough_routes"]) {
        const body = await read(path);
        expect(body).not.toContain(password);
        expect(body).toContain("svc:[REDACTED]@");
      }
    } finally {
      await fileApp?.exit();
      await etcd.deletePrefix(prefix);
    }
  });

  // An api key's display_name is its label: the `${request.api_key.name}`
  // header template and the `api_key_name` metric label read it. Moving the
  // configuration to a file must not lose it.
  test("an api key's label survives the move", async (ctx) => {
    if (!etcdReachable || !upstream) {
      ctx.skip();
      return;
    }
    const etcd = new EtcdClient();
    const prefix = `/aisix-export-label-${randomUUID()}`;
    const seed = new SeedClient(etcd, prefix);
    const caller = "sk-export-label-caller";
    let labelApp: SpawnedApp | undefined;
    try {
      const pk = await seed.createProviderKey({
        display_name: "label-pk",
        api_key: "sk-label-secret",
        api_base: `${upstream.baseUrl}/v1`,
        request: { default_headers: { "x-caller-key": "${request.api_key.name}" } },
      });
      await seed.createModel({
        display_name: "label-model",
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
      await seed.createApiKey({
        key_hash: createHash("sha256").update(caller).digest("hex"),
        display_name: "billing-service",
        allowed_models: ["label-model"],
      });

      const { stdout: yaml } = await execFileP(BIN_PATH, [
        "export",
        "--etcd",
        ETCD_ENDPOINT,
        "--prefix",
        prefix,
      ]);
      // A default header can carry a credential, so the export writes every
      // one as a placeholder; the operator puts the template back through
      // its variable like any other value.
      labelApp = await spawnApp({
        resourcesFile: yaml,
        extraEnv: Object.fromEntries(
          placeholderVars(yaml).map((v) => [
            v,
            v.endsWith("X_CALLER_KEY") ? "${request.api_key.name}" : "sk-label-secret",
          ]),
        ),
      });
      const before = upstream.receivedRequests.length;
      const chat = await new ProxyClient(labelApp.proxyUrl, caller).chat({
        model: "label-model",
        messages: [{ role: "user", content: "who is calling" }],
      });
      expect(chat.status).toBe(200);
      const sent = upstream.receivedRequests.slice(before);
      expect(sent).toHaveLength(1);
      expect(sent[0]!.headers["x-caller-key"]).toBe("billing-service");
    } finally {
      await labelApp?.exit();
      await etcd.deletePrefix(prefix);
    }
  });

  // Anonymous MCP access runs as an api key named by id, and in a file every
  // api key id is derived from the key's name: exported verbatim, the etcd id
  // names no key and anonymous access closes. The resource URL may carry a
  // password in its userinfo, which the default export must not print.
  test("anonymous MCP access and its resource URL survive the move", async (ctx) => {
    if (!etcdReachable) {
      ctx.skip();
      return;
    }
    const etcd = new EtcdClient();
    const prefix = `/aisix-export-anon-${randomUUID()}`;
    const seed = new SeedClient(etcd, prefix);
    const password = "resource-url-password-must-not-leak-7q6w";
    let docs: McpUpstream | undefined;
    let anonApp: SpawnedApp | undefined;
    try {
      docs = await startMcpUpstream("docs");
      await seed.update("mcp_servers", randomUUID(), {
        name: "docs",
        url: docs.url,
        enabled: true,
      });
      const principal = await seed.createApiKey({
        key_hash: createHash("sha256").update(`sk-anon-${randomUUID()}`).digest("hex"),
        display_name: "anonymous-docs",
        allowed_models: [],
        mcp_access: { allow: ["*"] },
      });
      await seed.update("mcp_auth_settings", randomUUID(), {
        resource_url: `https://svc:${password}@gw.example.com/mcp`,
        anonymous: {
          enabled: true,
          api_key_id: principal.id,
          // The e2e client connects over loopback, in either family.
          source_cidrs: ["127.0.0.0/8", "::1/128"],
          servers: ["docs"],
        },
      });

      const { stdout: yaml, stderr } = await execFileP(BIN_PATH, [
        "export",
        "--etcd",
        ETCD_ENDPOINT,
        "--prefix",
        prefix,
      ]);
      expect(yaml).not.toContain(password);
      expect(stderr).not.toContain(password);
      const vars = placeholderVars(yaml);
      const urlVar = vars.find((v) => v.includes("MCP_AUTH_SETTINGS") && v.endsWith("_PASSWORD"));
      expect(urlVar, yaml).toBeDefined();

      anonApp = await spawnApp({
        resourcesFile: yaml,
        extraEnv: Object.fromEntries(vars.map((v) => [v, password])),
      });
      // No credential at all, from an allowed source: served as the bound
      // principal, exactly as the etcd-backed gateway would.
      const rpc = (body: unknown) =>
        fetch(`${anonApp!.proxyUrl}/mcp/docs`, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            accept: "application/json, text/event-stream",
          },
          body: JSON.stringify(body),
        });
      const init = await rpc({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-11-25",
          capabilities: {},
          clientInfo: { name: "export-anon-e2e", version: "0.1" },
        },
      });
      await init.text();
      expect(init.status).toBe(200);
      await (await rpc({ jsonrpc: "2.0", method: "notifications/initialized" })).text();
      const call = await rpc({
        jsonrpc: "2.0",
        id: 2,
        method: "tools/call",
        params: { name: "echo", arguments: { text: "anonymous after export" } },
      });
      const reply = await call.text();
      expect(call.status).toBe(200);
      expect(reply).toContain("anonymous after export");
    } finally {
      await anonApp?.exit();
      await docs?.close();
      await etcd.deletePrefix(prefix);
    }
  });

  test("an MCP policy the file cannot carry fails the export and names itself", async (ctx) => {
    if (!etcdReachable) {
      ctx.skip();
      return;
    }
    const etcd = new EtcdClient();
    const prefix = `/aisix-export-policy-${randomUUID()}`;
    const seed = new SeedClient(etcd, prefix);
    const policyId = randomUUID();
    try {
      await seed.update("mcp_policies", policyId, {
        scope: "env",
        allow: ["*"],
        deny: ["srv__drop_table"],
      });
      const exported = await execFileP(BIN_PATH, [
        "export",
        "--etcd",
        ETCD_ENDPOINT,
        "--prefix",
        prefix,
      ]).then(
        () => ({ failed: false, stderr: "" }),
        (e: { stderr: string }) => ({ failed: true, stderr: e.stderr }),
      );
      expect(exported.failed).toBe(true);
      expect(exported.stderr).toContain(policyId);
    } finally {
      await etcd.deletePrefix(prefix);
    }
  });

  test("default export redacts a live secret on every secret-bearing kind", async (ctx) => {
    if (!etcdReachable) {
      ctx.skip();
      return;
    }
    // A dedicated prefix seeded with one distinctively-marked secret per
    // redacted field/kind. Reads etcd directly — no gateway needed. Each
    // entry is asserted present (by name) AND its marker absent, so a
    // rejected-and-dropped seed can't make a leak assertion pass vacuously.
    const etcd = new EtcdClient();
    const prefix = `/aisix-export-noleak-${randomUUID()}`;
    const seed = new SeedClient(etcd, prefix);
    const marker = {
      providerApiKey: "leak-provider-apikey-AAA111",
      providerHeader: "leak-provider-header-BBB222",
      mcpSecret: "leak-mcp-secret-CCC333",
      guardrailApiKey: "leak-guardrail-apikey-DDD444",
      otlpHeader: "leak-otlp-header-EEE555",
    };
    try {
      await seed.createProviderKey({
        display_name: "noleak-pk",
        api_key: marker.providerApiKey,
        request: { default_headers: { "x-tenant-token": marker.providerHeader } },
      });
      await seed.update("mcp_servers", randomUUID(), {
        name: "noleak-mcp",
        url: "https://mcp.example.com/mcp",
        auth_type: "bearer",
        secret: marker.mcpSecret,
      });
      const guardrail = await seed.createGuardrail({
        name: "noleak-guardrail",
        kind: "openai_moderation",
        api_key: marker.guardrailApiKey,
      }, { attach: false });
      // Attach it env-wide explicitly (hence `attach: false` above — this
      // test writes the attachment it means). The exporter emits the
      // guardrail and this attachment, exercising guardrail-credential
      // redaction on the round-trip.
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: guardrail.id,
        scope_type: "env",
        priority: 1,
      });
      await seed.createObservabilityExporter({
        name: "noleak-otlp",
        kind: "otlp_http",
        endpoint: "https://otlp.example.com/v1/traces",
        headers: { "x-honeycomb-team": marker.otlpHeader },
      });

      const { stdout: yaml, stderr } = await execFileP(BIN_PATH, [
        "export",
        "--etcd",
        ETCD_ENDPOINT,
        "--prefix",
        prefix,
      ]);

      // Each entry made it into the export (not silently dropped)…
      for (const name of ["noleak-pk", "noleak-mcp", "noleak-guardrail", "noleak-otlp"]) {
        expect(yaml, `${name} missing from export`).toContain(name);
      }
      // …and not one live secret survived at default settings, in the
      // file or in the stderr diagnostics.
      for (const [field, value] of Object.entries(marker)) {
        expect(yaml, `secret leaked to file: ${field}`).not.toContain(value);
        expect(stderr, `secret leaked to stderr: ${field}`).not.toContain(value);
      }
      expect(yaml).toContain("${AISIXSECRET");
      expect(stderr).toContain("secret value(s) were replaced");
    } finally {
      await etcd.deletePrefix(prefix);
    }
  });
});
