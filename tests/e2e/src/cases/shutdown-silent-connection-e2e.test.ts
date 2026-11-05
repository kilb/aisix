import { connect, type Socket } from "node:net";
import { afterEach, describe, expect, test } from "vitest";
import {
  spawnApp,
  startOpenAiUpstream,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a connection that never sends anything does not hold the process up.
//
// Anyone who can reach the port can open one, and a load balancer's preconnect
// pool does it routinely. It carries no request, so nothing about it is owed
// an answer, and a gateway that waits for it never exits on its own: the
// platform kills it, and everything an orderly exit does after the listeners
// close (delivering queued usage and flushing the log) is lost with it.
//
// The default configuration is what matters here: it sets no idle timeout, so
// nothing else would ever end such a connection.

const CALLER_KEY_ENV = "SILENT_CONNECTION_CALLER_KEY";

function resources(upstreamBase: string): string {
  return `
_format_version: "1"
provider_keys:
  - display_name: silent-connection-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstreamBase}/v1
models:
  - display_name: silent-connection-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: silent-connection-pk
api_keys:
  - display_name: silent-connection-caller
    key_env: ${CALLER_KEY_ENV}
    allowed_models: ["silent-connection-model"]
`;
}

function open(url: string): Promise<Socket> {
  const { hostname, port } = new URL(url);
  return new Promise((resolve, reject) => {
    const socket = connect(Number(port), hostname, () => resolve(socket));
    socket.once("error", reject);
  });
}

describe("a silent connection at exit", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  const sockets: Socket[] = [];

  afterEach(async () => {
    for (const socket of sockets.splice(0)) socket.destroy();
    await app?.exit();
    await upstream?.close();
    app = undefined;
    upstream = undefined;
  });

  for (const threadPerCore of [false, true]) {
    test(`does not keep the gateway from exiting (thread_per_core=${threadPerCore})`, async () => {
      upstream = await startOpenAiUpstream();
      app = await spawnApp({
        threadPerCore,
        // The clean-exit line is reported at INFO; the suite default is WARN.
        logLevel: "info",
        resourcesFile: resources(upstream.baseUrl),
        extraEnv: { [CALLER_KEY_ENV]: "sk-silent-connection-caller" },
      });

      sockets.push(await open(app.proxyUrl), await open(app.metricsUrl));
      // Accepted before the signal, so the listeners have taken them on.
      await new Promise((r) => setTimeout(r, 200));

      app.signal("SIGTERM");
      await app.waitForExit(30_000);
      expect(app.output()).toContain("aisix shut down cleanly");
    });
  }
});
