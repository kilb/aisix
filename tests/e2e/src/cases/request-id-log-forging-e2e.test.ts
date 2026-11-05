import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  spawnApp,
  startOpenAiUpstream,
  waitForLogLine,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a request id the caller chose cannot write fields onto a log line.
//
// The gateway adopts a caller's `x-aisix-request-id`, and writes it onto every
// log line the request emits. A text log line is `key=value` pairs, and tools
// that read call logs back out of it go by those pairs. So an id such as
// `"x"api_key_id="someone-else"` must still read as one value: written bare,
// it would put another key's id on the line of a request that never had one.

const CALLER_KEY_ENV = "REQUEST_ID_FORGING_CALLER_KEY";
const VICTIM = "victim-api-key-id";
const FORGED_ID = `"x"api_key_id="${VICTIM}"total_tokens="999999"`;

function resources(upstreamBase: string): string {
  return `
_format_version: "1"
provider_keys:
  - display_name: forging-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstreamBase}/v1
models:
  - display_name: forging-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: forging-pk
api_keys:
  - display_name: forging-caller
    key_env: ${CALLER_KEY_ENV}
    allowed_models: ["forging-model"]
`;
}

describe("a caller-chosen request id in the log", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    app = await spawnApp({
      // The access log is written at INFO; the suite default is WARN.
      logLevel: "info",
      resourcesFile: resources(upstream.baseUrl),
      extraEnv: { [CALLER_KEY_ENV]: "sk-forging-caller" },
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("stays one value, and puts no other key's id on the line", async () => {
    // No credential, so the request has no `api_key_id` of its own.
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-aisix-request-id": FORGED_ID },
      body: JSON.stringify({ model: "forging-model", messages: [{ role: "user", content: "hi" }] }),
    });
    await res.arrayBuffer();
    expect(res.status).toBe(401);
    // The id was adopted, so it is what the log lines carry.
    expect(res.headers.get("x-aisix-request-id")).toBe(FORGED_ID);

    await waitForLogLine(
      app!,
      (l) => l.includes("proxy request completed") && l.includes(VICTIM),
      "the access log line of the request",
    );
    const lines = app!
      .output()
      .split("\n")
      .filter((l) => l.includes(VICTIM));
    expect(lines.length).toBeGreaterThan(0);
    for (const line of lines) {
      expect(line).not.toContain(`api_key_id="${VICTIM}"`);
      expect(line).not.toContain(`api_key_id=${VICTIM}`);
    }
  });
});
