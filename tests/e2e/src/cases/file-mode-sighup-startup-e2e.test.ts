import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, open, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, describe, expect, test } from "vitest";
import { stringify as yamlStringify } from "yaml";
import { pickFreePorts, startOpenAiUpstream, suiteThreadPerCore, type OpenAiUpstream } from "../harness/index.js";

const BIN_PATH =
  process.env.AISIX_BIN ?? join(process.cwd(), "..", "..", "target", "debug", "aisix");

// E2E: a file-mode gateway that receives SIGHUP while it is still starting
// up keeps starting, and serves.
//
// File mode is the one a co-located portal reloads by writing the resources
// file and broadcasting `pkill -HUP`. A write that lands while a gateway is
// restarting sends that SIGHUP into the startup — and if the gateway only
// installs its handler once the first load has finished, the signal's
// default action terminates it, silently, with nothing in its log.
//
// The resources file here is a named pipe, so the gateway's first load
// blocks until this test writes it: the gateway is provably mid-startup
// (it has opened the file) when the signal arrives.

const CALLER = "sk-file-sighup-startup";
const CALLER_ENV = "FILE_SIGHUP_STARTUP_CALLER_KEY";
/** How long the process must stay up after the signal; the default action is immediate. */
const SURVIVAL_WINDOW_MS = 1_500;

const resources = (upstreamBase: string) => `
_format_version: "1"
provider_keys:
  - display_name: fs-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstreamBase}/v1
models:
  - display_name: fs-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: fs-pk
api_keys:
  - display_name: fs-caller
    key_env: ${CALLER_ENV}
    allowed_models: ["fs-model"]
`;

/** Hand `body` to the gateway through the pipe: resolves once the gateway has opened it and read it all. */
async function feed(fifo: string, body: string): Promise<void> {
  const fh = await open(fifo, "w");
  try {
    await fh.writeFile(body, "utf8");
  } finally {
    await fh.close();
  }
}

describe("file resource source: SIGHUP during startup", () => {
  let child: ChildProcess | undefined;
  let upstream: OpenAiUpstream | undefined;
  let dir: string | undefined;

  afterAll(async () => {
    if (child && child.exitCode === null && child.signalCode === null) {
      const exited = new Promise<void>((resolve) => child?.once("exit", () => resolve()));
      child.kill("SIGKILL");
      await exited;
    }
    await upstream?.close();
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  test("a SIGHUP while the gateway loads its resources file does not terminate it", async () => {
    upstream = await startOpenAiUpstream();
    dir = await mkdtemp(join(tmpdir(), "aisix-file-sighup-boot-"));
    const fifo = join(dir, "resources.yaml");
    execFileSync("mkfifo", [fifo]);

    const [proxyPort, adminPort, metricsPort] = await pickFreePorts(3);
    const cfg = {
      resources_file: fifo,
      proxy: {
        addr: `127.0.0.1:${proxyPort}`,
        ...(suiteThreadPerCore !== undefined ? { thread_per_core: suiteThreadPerCore } : {}),
      },
      admin: { addr: `127.0.0.1:${adminPort}`, enabled: false },
      observability: {
        service_name: "aisix-file-sighup-boot",
        log_level: "warn",
        metrics: {
          prometheus: { enabled: true, path: "/metrics", addr: `127.0.0.1:${metricsPort}` },
          otlp: { enabled: false, endpoint: "http://127.0.0.1:4317" },
        },
        tracing: { otlp: { enabled: false, endpoint: "http://127.0.0.1:4317", sample_ratio: 1 } },
      },
      cache: { backend: "memory" },
    };
    const cfgPath = join(dir, "config.yaml");
    await writeFile(cfgPath, yamlStringify(cfg), "utf8");

    const childEnv: Record<string, string> = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (v !== undefined && !k.startsWith("AISIX_")) childEnv[k] = v;
    }
    childEnv.RUST_LOG = "warn";
    childEnv[CALLER_ENV] = CALLER;

    let output = "";
    const gateway = spawn(BIN_PATH, ["--config", cfgPath], { stdio: ["ignore", "pipe", "pipe"], env: childEnv });
    child = gateway;
    gateway.stdout?.on("data", (c: Buffer) => (output += c.toString("utf8")));
    gateway.stderr?.on("data", (c: Buffer) => (output += c.toString("utf8")));
    const exited = new Promise<string>((resolve) =>
      gateway.once("exit", (code, signal) => resolve(`exited (code=${code} signal=${signal})`)),
    );

    // Opening the pipe for writing returns once the gateway has opened it to
    // read: it is in the middle of its first load, not serving yet.
    const writer = open(fifo, "w");
    const opened = await Promise.race([
      writer.then(() => "opened"),
      exited,
      sleep(20_000).then(() => "the gateway never opened its resources file within 20s"),
    ]);
    expect(opened, output).toBe("opened");
    await expect(
      fetch(`http://127.0.0.1:${proxyPort}/livez`),
      "the gateway was already serving; this case needs it mid-startup",
    ).rejects.toThrow();

    gateway.kill("SIGHUP");
    const outcome = await Promise.race([exited, sleep(SURVIVAL_WINDOW_MS).then(() => "running")]);
    expect(outcome, `the gateway exited on a SIGHUP during startup:\n${output}`).toBe("running");

    // Let the first load finish; the gateway then serves.
    const fh = await writer;
    await fh.writeFile(resources(upstream.baseUrl), "utf8");
    await fh.close();
    // The SIGHUP that arrived during startup asks for one reload, which reads
    // the file again.
    const reloaded = feed(fifo, resources(upstream.baseUrl));
    let status = 0;
    for (let i = 0; i < 100 && status !== 200; i++) {
      try {
        const res = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
          method: "POST",
          headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
          body: JSON.stringify({ model: "fs-model", messages: [{ role: "user", content: "hi" }] }),
        });
        status = res.status;
        await res.text();
      } catch {
        await sleep(100);
      }
    }
    expect(status, output).toBe(200);
    await Promise.race([reloaded, sleep(10_000)]);
    expect(gateway.exitCode, output).toBeNull();
    expect(gateway.signalCode, output).toBeNull();
  }, 60_000);
});
