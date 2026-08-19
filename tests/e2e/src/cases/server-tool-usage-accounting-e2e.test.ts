import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  decodedTextFor,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForToken,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// Anthropic bills web search PER SEARCH on top of tokens
// (`usage.server_tool_use.web_search_requests`, $10/1000 —
// https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool).
// The gateway parsed none of it, so a request that cost real money beyond
// its tokens looked, in the usage record, exactly like one that did not:
// the spend was invisible to anything computing cost downstream.
//
// Web fetch reports the same way and costs nothing extra. It is carried
// because it says the model reached out to a URL — the signal an operator
// wants from a tool whose own documentation calls it an exfiltration
// vector.
//
// The assertion is on the record the gateway SHIPS, decoded off an exporter
// endpoint. A counter existing on a struct proves nothing about what leaves
// the process.

const CALLER = "sk-server-tool-usage-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

const SLS_PROJECT = "aisix-e2e-stu";
const SLS_LOGSTORE = "stu-usage";
const CREDENTIAL_REF = "mock";
const MOCK_AK_ID = "mock-ak-id";
const MOCK_AK_SECRET = "mock-ak-secret";

describe("server-tool usage accounting", () => {
  let app: SpawnedApp | undefined;
  let searched: OpenAiUpstream | undefined;
  let plain: OpenAiUpstream | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Reports two searches and one fetch alongside its tokens.
    searched = await startOpenAiUpstream({
      nonStreamBody: anthropicBody("searched", {
        input_tokens: 11,
        output_tokens: 7,
        server_tool_use: { web_search_requests: 2, web_fetch_requests: 1 },
      }),
    });
    // Reports no `server_tool_use` block at all — the common case.
    plain = await startOpenAiUpstream({
      nonStreamBody: anthropicBody("no tools", {
        input_tokens: 3,
        output_tokens: 2,
      }),
    });
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: MOCK_AK_ID,
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: MOCK_AK_SECRET,
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    await seed.createObservabilityExporter({
      name: "stu-sls",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: SLS_LOGSTORE,
      credential_ref: CREDENTIAL_REF,
    });
    for (const [name, up] of [
      ["stu-searched", searched],
      ["stu-plain", plain],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: up.baseUrl,
        provider: "anthropic",
        adapter: "anthropic",
      });
      await seed.createModel({
        display_name: name,
        provider: "anthropic",
        model_name: "claude-3-5-haiku-20241022",
        provider_key_id: pk.id,
      });
    }
    // Last, so a gate on this key authenticating implies every row above.
    await seed.createApiKey({
      key_hash: sha256(CALLER),
      allowed_models: ["stu-searched", "stu-plain"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await searched?.close();
    await plain?.close();
    await sls?.close();
  });

  // ONE test, and the order inside it is load-bearing: the exporter batches,
  // so a window opened after searched traffic can still receive a batch that
  // straddles it. Asserting the ABSENCE of a field is only sound in a window
  // no searched request could have reached — which means running the
  // tool-free case first, before any such request exists.
  test("server-tool counts ride the shipped usage record", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }

    // Gate on the tool-free model, so no searched traffic exists yet.
    await waitConfigPropagation(
      async () => (await messages("stu-plain")).status === 200,
    );

    const beforePlain = sls.requests.length;
    expect((await messages("stu-plain")).status).toBe(200);
    await waitForToken(sls, SLS_LOGSTORE, "stu-plain", 15_000, beforePlain);
    const plainShipped = decodedTextFor(sls, SLS_LOGSTORE, beforePlain);

    expect(
      plainShipped.includes("web_search_requests"),
      "a zero must be omitted from the wire, not sent as 0 — the common " +
        "event shape has to stay exactly what an older control plane " +
        `already parses: ${plainShipped.slice(0, 400)}`,
    ).toBe(false);
    expect(plainShipped.includes("web_fetch_requests")).toBe(false);

    // Only now does a searched request exist.
    const beforeSearched = sls.requests.length;
    expect((await messages("stu-searched")).status).toBe(200);
    await waitForToken(sls, SLS_LOGSTORE, "stu-searched", 15_000, beforeSearched);
    const shipped = decodedTextFor(sls, SLS_LOGSTORE, beforeSearched);

    expect(
      shipped,
      "a per-search charge the gateway never counted is spend nobody can " +
        `see: ${shipped.slice(0, 400)}`,
    ).toContain("web_search_requests");
    expect(shipped).toMatch(/web_search_requests\D+2/);
    expect(shipped).toMatch(/web_fetch_requests\D+1/);
    // The token counters are untouched — a per-search charge is its own cost
    // basis, not a token class, so folding it into a token total would both
    // inflate the tokens and hide the charge.
    expect(shipped).toMatch(/prompt_tokens\D+11/);
    expect(shipped).toMatch(/completion_tokens\D+7/);
  }, 120_000);

  async function messages(model: string): Promise<{ status: number }> {
    const res = await fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "x-api-key": CALLER, "content-type": "application/json" },
      body: JSON.stringify({
        model,
        max_tokens: 64,
        messages: [{ role: "user", content: "what is the weather?" }],
        tools: [{ type: "web_search_20250305", name: "web_search" }],
      }),
    });
    await res.text();
    return { status: res.status };
  }
});

function anthropicBody(text: string, usage: Record<string, unknown>) {
  return {
    id: `msg_${text.replace(/\s/g, "_")}`,
    type: "message",
    role: "assistant",
    model: "claude-3-5-haiku-20241022",
    content: [{ type: "text", text }],
    stop_reason: "end_turn",
    usage,
  };
}
