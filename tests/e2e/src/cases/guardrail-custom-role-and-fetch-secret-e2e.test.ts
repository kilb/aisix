import { createHash } from "node:crypto";
import { createServer } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForLogLine,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E for two promises a `custom` guardrail makes to the operator who writes
// its script.
//
// R-054: `ctx.messages[].role` names who wrote each message. On the output
// hook every message is the model's answer, so a rule that keys on the
// assistant's messages must fire on a chat answer and a Messages answer —
// the paths that hand the answer to the script as segments.
//
// R-087: `ctx.secrets` values are never logged. A script that builds its
// screening URL from a secret and does not catch a failed `fetch` has the
// gateway log the rejection as the error the script threw; that line must
// not carry the URL's query. The request still fails closed.

const ROLE_CALLER = "sk-custom-role-caller";
const FETCH_CALLER = "sk-custom-fetch-caller";
const ROLE_MARKER = "customrolemarker";
const SECRET = "sk-query-secret-0087";
const hash = (s: string) => createHash("sha256").update(s).digest("hex");

/** A port nothing listens on: bound, then released. */
async function refusedPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as { port: number };
  await new Promise<void>((resolve) => server.close(() => resolve()));
  return port;
}

describe("custom guardrail: output messages are the assistant's, and fetch failures log no secret", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-role",
        object: "chat.completion",
        created: 1,
        model: "gpt-4o-mini",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: `the ${ROLE_MARKER} answer` },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 3, completion_tokens: 3, total_tokens: 6 },
      },
    });
    const dead = await refusedPort();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "custom-role-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    const roleModel = await seed.createModel({
      display_name: "custom-role-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    const fetchModel = await seed.createModel({
      display_name: "custom-fetch-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    const roleRule = await seed.createGuardrail(
      {
        name: "custom-assistant-role",
        enabled: true,
        kind: "custom",
        hook_point: "output",
        output_fail_open: false,
        timeout_ms: 5000,
        script: `export function checkOutput(ctx) {
          const answered = ctx.messages.some(
            (m) => m.role === "assistant" && m.text.includes("${ROLE_MARKER}"),
          );
          return answered ? { action: "block", reason_code: "assistant_marker" } : { action: "none" };
        }`,
      },
      { attach: false },
    );
    await seed.attachGuardrailToModel(roleRule.id as string, roleModel.id as string);
    const fetchRule = await seed.createGuardrail(
      {
        name: "custom-fetch-secret",
        enabled: true,
        kind: "custom",
        hook_point: "input",
        fail_open: false,
        timeout_ms: 5000,
        secrets: { SCAN_KEY: SECRET },
        script: `export async function checkInput(ctx) {
          await fetch("http://127.0.0.1:${dead}/scan?key=" + ctx.secrets.SCAN_KEY);
          return { action: "none" };
        }`,
      },
      { attach: false },
    );
    await seed.attachGuardrailToModel(fetchRule.id as string, fetchModel.id as string);
    // Keys last: once the last one authenticates, the rest is in the snapshot.
    await seed.createApiKey({ key_hash: hash(ROLE_CALLER), allowed_models: ["custom-role-model"] });
    await seed.createApiKey({ key_hash: hash(FETCH_CALLER), allowed_models: ["custom-fetch-model"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${FETCH_CALLER}` },
      });
      await res.text();
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("chat answer: a rule on the assistant's messages blocks it", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { authorization: `Bearer ${ROLE_CALLER}`, "content-type": "application/json" },
      body: JSON.stringify({
        model: "custom-role-model",
        messages: [{ role: "user", content: "say it" }],
      }),
    });
    const body = await res.text();
    expect(body).not.toContain(ROLE_MARKER);
    expect(res.status, body).toBe(422);
  });

  test("Messages answer: a rule on the assistant's messages blocks it", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": ROLE_CALLER,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "custom-role-model",
        max_tokens: 32,
        messages: [{ role: "user", content: "say it" }],
      }),
    });
    const body = await res.text();
    expect(body).not.toContain(ROLE_MARKER);
    expect(res.status, body).toBe(422);
  });

  test("an uncaught failed fetch fails closed and its log carries no query secret", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { authorization: `Bearer ${FETCH_CALLER}`, "content-type": "application/json" },
      body: JSON.stringify({
        model: "custom-fetch-model",
        messages: [{ role: "user", content: "hello" }],
      }),
    });
    const body = await res.text();
    expect(res.status, body).toBe(422);
    // The rejection is logged after the fetch failure through the same FIFO
    // writer, so once its line is visible every line that could carry the
    // secret is too.
    const threw = await waitForLogLine(
      app!,
      (line) => line.includes("custom guardrail script threw"),
      "the script's uncaught rejection",
    );
    expect(threw).not.toContain(SECRET);
    expect(app!.output()).not.toContain(SECRET);
  });
});
