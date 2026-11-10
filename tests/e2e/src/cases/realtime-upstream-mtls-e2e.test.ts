import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { createServer as createHttpsServer, type Server } from "node:https";
import type { TLSSocket } from "node:tls";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { WebSocket as WsClient, WebSocketServer } from "ws";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: the Realtime upstream WebSocket presents the deployment's client
// certificate (`upstream.tls.client_cert_file` / `client_key_file`) to an
// upstream that requires mutual TLS, as every HTTP upstream call does.
//
// The upstream here refuses any TLS client that does not present a
// certificate signed by the client CA, so a session reaches it only when the
// gateway presented the configured identity. It records the subject of the
// certificate it was shown.

const CALLER = "sk-realtime-upstream-mtls";
const CLIENT_CN = "aisix-gateway-client";

interface Pki {
  dir: string;
  serverKey: Buffer;
  serverCert: Buffer;
  clientCa: Buffer;
  clientCertPath: string;
  clientKeyPath: string;
}

/** A self-signed server certificate for 127.0.0.1, and a client CA with a
 * client certificate it signs. */
function makePki(): Pki {
  const dir = mkdtempSync(join(tmpdir(), "aisix-rt-mtls-"));
  const p = (name: string) => join(dir, name);
  const openssl = (args: string[]) => execFileSync("openssl", args, { stdio: "ignore" });
  openssl([
    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
    "-keyout", p("server.key"), "-out", p("server.crt"),
    "-subj", "/CN=127.0.0.1",
    "-addext", "subjectAltName=IP:127.0.0.1,DNS:localhost",
  ]);
  openssl([
    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
    "-keyout", p("client-ca.key"), "-out", p("client-ca.crt"),
    "-subj", "/CN=aisix-e2e-client-ca",
  ]);
  openssl(["genrsa", "-out", p("client.key"), "2048"]);
  openssl([
    "req", "-new", "-key", p("client.key"), "-out", p("client.csr"),
    "-subj", `/CN=${CLIENT_CN}`,
  ]);
  openssl([
    "x509", "-req", "-in", p("client.csr"),
    "-CA", p("client-ca.crt"), "-CAkey", p("client-ca.key"), "-CAcreateserial",
    "-out", p("client.crt"), "-days", "1", "-sha256",
  ]);
  return {
    dir,
    serverKey: readFileSync(p("server.key")),
    serverCert: readFileSync(p("server.crt")),
    clientCa: readFileSync(p("client-ca.crt")),
    clientCertPath: p("client.crt"),
    clientKeyPath: p("client.key"),
  };
}

interface Upstream {
  port: number;
  /** The subject CN of the client certificate each upgrade presented. */
  presented: string[];
  close(): Promise<void>;
}

/** A `wss://` Realtime upstream that requires a client certificate signed by
 * the client CA, and answers the first client event with `response.done`. */
async function startMtlsUpstream(pki: Pki): Promise<Upstream> {
  const server: Server = createHttpsServer({
    key: pki.serverKey,
    cert: pki.serverCert,
    ca: pki.clientCa,
    requestCert: true,
    rejectUnauthorized: true,
  });
  const wss = new WebSocketServer({ server });
  const presented: string[] = [];
  wss.on("connection", (socket, req) => {
    const peer = (req.socket as TLSSocket).getPeerCertificate();
    presented.push(String(peer?.subject?.CN ?? ""));
    socket.on("message", () =>
      socket.send(
        JSON.stringify({
          type: "response.done",
          response: { usage: { input_tokens: 3, output_tokens: 2 } },
        }),
      ),
    );
  });
  const port = await new Promise<number>((resolve) =>
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (addr === null || typeof addr === "string") throw new Error("no port");
      resolve(addr.port);
    }),
  );
  return {
    port,
    presented,
    close: () => {
      for (const c of wss.clients) c.terminate();
      return new Promise((resolve) => {
        wss.close();
        server.closeAllConnections();
        server.close(() => resolve());
      });
    },
  };
}

/** Open a session on `model`, send one event, and return the first frame the
 * gateway relays back. */
function firstFrame(app: SpawnedApp, model: string): Promise<{ type: string }> {
  const url = `${app.proxyUrl.replace("http://", "ws://")}/v1/realtime?model=${model}`;
  const c = new WsClient(url, { headers: { authorization: `Bearer ${CALLER}` } });
  return new Promise((resolve, reject) => {
    c.on("open", () => c.send(JSON.stringify({ type: "session.update" })));
    c.on("message", (d) => {
      resolve(JSON.parse(d.toString()));
      c.terminate();
    });
    c.on("close", (code) => reject(new Error(`session closed (${code}) before a frame`)));
    c.on("unexpected-response", (_q, res) => reject(new Error(`upgrade refused: ${res.statusCode}`)));
    c.on("error", reject);
  });
}

describe("realtime upstream WebSocket presents the configured client certificate", () => {
  let app: SpawnedApp | undefined;
  let upstream: Upstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    const pki = makePki();
    upstream = await startMtlsUpstream(pki);
    app = await spawnApp({
      extra: {
        upstream: {
          tls: {
            // The server certificate is self-signed; what is under test is
            // the identity the gateway presents, not the one it checks.
            verify: false,
            client_cert_file: pki.clientCertPath,
            client_key_file: pki.clientKeyPath,
          },
        },
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "rt-mtls-pk",
      secret: "sk-upstream-realtime",
      api_base: `https://127.0.0.1:${upstream.port}/v1`,
    });
    await seed.createModel({
      display_name: "rt-mtls",
      provider: "openai",
      model_name: "gpt-realtime-mock",
      provider_key_id: pk.id,
    });
    // Seeded last: once it authenticates, every resource above is live.
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      allowed_models: ["*"],
    });
    const client = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await client.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a session reaches an upstream that requires mutual TLS, presenting the configured certificate", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const frame = await firstFrame(app, "rt-mtls");
    expect(frame.type).toBe("response.done");
    expect(upstream.presented).toEqual([CLIENT_CN]);
  });
});
