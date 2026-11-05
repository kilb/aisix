import { afterEach, describe, expect, test } from "vitest";
import {
  spawnApp,
  startOpenAiUpstream,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";
import { startMockOtlp, type MockOtlp } from "../harness/otlp-mock.js";

// E2E: stopping the gateway delivers what its exporters still hold.
//
// A record that has been queued for a trace backend is owed to that backend.
// A rolling restart answers its last requests and exits within the same
// second, so if the exit does not deliver the queue, the requests served last
// by every replica are missing from the backend after every deploy.
//
// Both serving modes, because they differ in where an exporter's delivery
// worker could live: on the shared runtime, or on the thread of the request
// worker that happened to serve the exporter's first event.

const CALLER_PLAINTEXT = "sk-exporter-drain-caller";
const CALLER_KEY_ENV = "EXPORTER_DRAIN_CALLER_KEY";
const MODEL = "exporter-drain-model";

function resources(upstreamBase: string, exporterUrl: string): string {
  return `
_format_version: "1"
provider_keys:
  - display_name: exporter-drain-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstreamBase}/v1
models:
  - display_name: ${MODEL}
    provider: openai
    model_name: gpt-4o-mini
    provider_key: exporter-drain-pk
api_keys:
  - display_name: exporter-drain-caller
    key_env: ${CALLER_KEY_ENV}
    allowed_models: ["${MODEL}"]
observability_exporters:
  - name: exporter-drain-backend
    kind: otlp_http
    endpoint: ${exporterUrl}
`;
}

describe("an exit delivers what the exporters still hold", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let receiver: MockOtlp | undefined;

  afterEach(async () => {
    await app?.exit();
    await receiver?.close();
    await upstream?.close();
    app = undefined;
    receiver = undefined;
    upstream = undefined;
  });

  for (const threadPerCore of [false, true]) {
    test(`a request served just before SIGTERM reaches the backend (thread_per_core=${threadPerCore})`, async () => {
      upstream = await startOpenAiUpstream();
      receiver = await startMockOtlp();
      app = await spawnApp({
        threadPerCore,
        resourcesFile: resources(upstream.baseUrl, receiver.url),
        extraEnv: { [CALLER_KEY_ENV]: CALLER_PLAINTEXT },
      });

      const chat = async () => {
        const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${CALLER_PLAINTEXT}`,
            "content-type": "application/json",
          },
          body: JSON.stringify({
            model: MODEL,
            messages: [{ role: "user", content: "hi" }],
          }),
        });
        expect(res.status).toBe(200);
        await res.arrayBuffer();
      };

      // The first event goes out as soon as the exporter's worker starts, so
      // it says nothing about the exit. It is the one after it that is still
      // queued when the signal arrives.
      await chat();
      const deadline = Date.now() + 10_000;
      while (receiver.posts === 0 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 5));
      }
      expect(receiver.posts).toBeGreaterThan(0);
      const spansBefore = receiver.spans.length;

      await chat();
      app.signal("SIGTERM");
      await app.waitForExit(30_000);

      expect(receiver.parseFailures).toEqual([]);
      expect(receiver.spans.length).toBeGreaterThan(spansBefore);
    });
  }
});
