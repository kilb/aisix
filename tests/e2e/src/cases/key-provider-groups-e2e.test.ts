import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// An API key that names `allowed_provider_groups` is served only by upstreams
// whose provider key is in one of those groups. Nothing widens that: not the
// group's strategy spreading traffic, not a failing in-group upstream that a
// healthy out-of-group one could have answered, and not a routing-tag header
// that names an out-of-group target. Each case runs on every endpoint of the
// chat family, because the three dispatch separately.
//
// Group and provider-key names are distinctive tokens so the check that a
// refusal does not name them cannot trip over ordinary English text.

const GROUPED_PLAINTEXT = "sk-key-provider-groups-e2e-grouped";
const OPEN_PLAINTEXT = "sk-key-provider-groups-e2e-open";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

const LEAKS = ["grp-alpha", "grp-beta", "pk-"];

const CHAT_BODY = {
  id: "chatcmpl-groups",
  object: "chat.completion",
  created: 1,
  model: "gpt-4o-mini",
  choices: [
    {
      index: 0,
      message: { role: "assistant", content: "ok" },
      finish_reason: "stop",
    },
  ],
  usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
};

const RESPONSE_BODY = {
  id: "resp_groups",
  object: "response",
  status: "completed",
  model: "gpt-4o-mini",
  output: [
    {
      type: "message",
      id: "msg_groups",
      role: "assistant",
      status: "completed",
      content: [{ type: "output_text", text: "ok", annotations: [] }],
    },
  ],
  usage: { input_tokens: 3, output_tokens: 1, total_tokens: 4 },
};

// One mock answers whichever API the gateway calls it on: `/v1/messages` is
// translated to chat completions for an OpenAI-compatible upstream, and
// `/v1/responses` goes to the upstream's own `/v1/responses`.
const PATH_BODIES = {
  "/v1/chat/completions": CHAT_BODY,
  "/v1/responses": RESPONSE_BODY,
};

type Endpoint = "chat" | "messages" | "responses";
const ENDPOINTS: Endpoint[] = ["chat", "messages", "responses"];

interface Answer {
  status: number;
  text: string;
}

describe("an API key routes only within its allowed provider groups", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  // Healthy pair behind `groups-route`.
  let mockA: OpenAiUpstream | undefined;
  let mockB: OpenAiUpstream | undefined;
  // Pair behind `groups-route-down`: the in-group upstream answers 500.
  let mockADown: OpenAiUpstream | undefined;
  let mockBUp: OpenAiUpstream | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    mockA = await startOpenAiUpstream({ pathBodies: PATH_BODIES });
    mockB = await startOpenAiUpstream({ pathBodies: PATH_BODIES });
    mockADown = await startOpenAiUpstream({
      status: 500,
      errorBody: { error: { message: "upstream failed", type: "server_error" } },
    });
    mockBUp = await startOpenAiUpstream({ pathBodies: PATH_BODIES });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const model = async (
      name: string,
      pkName: string,
      groups: string[],
      upstream: OpenAiUpstream,
    ) => {
      const pk = await seed.createProviderKey({
        display_name: pkName,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
        groups,
      });
      await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
        retries: 0,
      });
    };
    await model("groups-model-a", "pk-alpha", ["grp-alpha"], mockA);
    await model("groups-model-b", "pk-beta", ["grp-beta"], mockB);
    await model("groups-model-a-down", "pk-alpha-down", ["grp-alpha"], mockADown);
    await model("groups-model-b-up", "pk-beta-up", ["grp-beta"], mockBUp);

    // Round robin, so without the group constraint traffic splits across
    // both targets. The tags name the groups, so a header asking for
    // `grp-beta` would select the out-of-group target on its own.
    await seed.createModel({
      display_name: "groups-route",
      routing: {
        strategy: "round_robin",
        targets: [
          { model: "groups-model-a", tags: ["grp-alpha"] },
          { model: "groups-model-b", tags: ["grp-beta"] },
        ],
      },
    });
    await seed.createModel({
      display_name: "groups-route-down",
      routing: {
        strategy: "round_robin",
        targets: [{ model: "groups-model-a-down" }, { model: "groups-model-b-up" }],
      },
    });

    // Caller keys last; the grouped key is the readiness gate below.
    await seed.createApiKey({
      key_hash: sha256(OPEN_PLAINTEXT),
      allowed_models: ["*"],
    });
    await seed.createApiKey({
      key_hash: sha256(GROUPED_PLAINTEXT),
      allowed_models: ["*"],
      allowed_provider_groups: ["grp-alpha"],
    });
    const probe = new ProxyClient(app.proxyUrl, GROUPED_PLAINTEXT);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(
      [mockA, mockB, mockADown, mockBUp].map((u) => u?.close()),
    );
  });

  async function send(
    endpoint: Endpoint,
    key: string,
    model: string,
    headers: Record<string, string> = {},
  ): Promise<Answer> {
    const [path, body, auth] =
      endpoint === "chat"
        ? [
            "/v1/chat/completions",
            { model, messages: [{ role: "user", content: "hi" }] },
            { authorization: `Bearer ${key}` },
          ]
        : endpoint === "messages"
          ? [
              "/v1/messages",
              { model, max_tokens: 5, messages: [{ role: "user", content: "hi" }] },
              { "x-api-key": key },
            ]
          : [
              "/v1/responses",
              { model, input: "hi" },
              { authorization: `Bearer ${key}` },
            ];
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...auth, ...headers },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  }

  function expectNoLeak(answer: Answer, label: string) {
    for (const token of LEAKS) {
      expect(answer.text, `${label}: body names ${token}`).not.toContain(token);
    }
  }

  test.for(ENDPOINTS)(
    "%s: every request lands in the key's group",
    async (endpoint, ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      const a0 = mockA!.receivedRequests.length;
      const b0 = mockB!.receivedRequests.length;
      for (let i = 0; i < 10; i++) {
        const answer = await send(endpoint, GROUPED_PLAINTEXT, "groups-route");
        expect(answer.status, `${endpoint} #${i}: ${answer.text}`).toBe(200);
      }
      expect(mockA!.receivedRequests.length - a0).toBe(10);
      expect(mockB!.receivedRequests.length - b0).toBe(0);
    },
  );

  test.for(ENDPOINTS)(
    "%s: a failing in-group upstream is not replaced by an out-of-group one",
    async (endpoint, ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      // What any key gets from the failing upstream on its own.
      const direct = await send(endpoint, OPEN_PLAINTEXT, "groups-model-a-down");
      expect(direct.status).toBeGreaterThanOrEqual(500);

      const down0 = mockADown!.receivedRequests.length;
      const up0 = mockBUp!.receivedRequests.length;
      const answer = await send(endpoint, GROUPED_PLAINTEXT, "groups-route-down");
      expect(answer.status, answer.text).toBe(direct.status);
      expect(mockADown!.receivedRequests.length - down0).toBeGreaterThanOrEqual(1);
      expect(mockBUp!.receivedRequests.length - up0).toBe(0);
      expectNoLeak(answer, endpoint);
    },
  );

  test.for(ENDPOINTS)(
    "%s: a routing-tag header cannot reach past the key's groups",
    async (endpoint, ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      const header = { "x-aisix-routing-tags": "grp-beta" };
      // The header does select the out-of-group target for a key without
      // groups, so what follows is the constraint at work.
      const b0 = mockB!.receivedRequests.length;
      const open = await send(endpoint, OPEN_PLAINTEXT, "groups-route", header);
      expect(open.status, open.text).toBe(200);
      expect(mockB!.receivedRequests.length - b0).toBe(1);

      const a1 = mockA!.receivedRequests.length;
      const b1 = mockB!.receivedRequests.length;
      const answer = await send(endpoint, GROUPED_PLAINTEXT, "groups-route", header);
      expect(mockB!.receivedRequests.length - b1).toBe(0);
      expect(mockA!.receivedRequests.length - a1).toBe(0);
      expect(answer.status, answer.text).toBe(503);
      expectNoLeak(answer, endpoint);
    },
  );
});
