import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";
import { pickFreePort } from "../harness/ports.js";

// E2E：流式 passthrough 路由上，下行心跳不能插进一个还没结束的 SSE 帧里。
//
// 上游最后一帧没有结束空行就关了连接；网关在流尾还要等一次输出护栏扫描
// （这里是一个慢的 monitor 模式远程审核），等待时间超过心跳间隔。心跳只能
// 出现在完整的帧之间：贴在半帧后面，客户端就会把 `data: {...}` 和心跳拼成
// 一行，最后一个事件变成一段坏 JSON。

const CALLER_PLAINTEXT = "sk-pt-tail-heartbeat-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

const HEARTBEAT_INTERVAL_S = 1;
const MODERATION_DELAY_MS = 2500;

interface Mock {
  baseUrl: string;
  close(): Promise<void>;
}

async function listen(server: Server): Promise<Mock> {
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    async close() {
      server.closeAllConnections?.();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

/** 上游：一个完整帧，然后一个没有结束空行的尾帧，随即关闭。 */
function startTailUpstream(): Promise<Mock> {
  return listen(
    createServer((req, res) => {
      req.resume();
      req.on("end", () => {
        res.writeHead(200, { "content-type": "text/event-stream" });
        // 带文本的帧：流尾扫描只看从帧里取出的文本，没有文本就不扫。
        res.write(`data: ${JSON.stringify({ choices: [{ delta: { content: "hello " } }] })}\n\n`);
        res.end(`data: ${JSON.stringify({ choices: [{ delta: { content: "world" } }] })}`);
      });
    }),
  );
}

/** 慢的审核上游：每次都放行，但要等 `MODERATION_DELAY_MS` 才回。 */
function startSlowModeration(): Promise<Mock> {
  return listen(
    createServer((req, res) => {
      req.resume();
      req.on("end", () => {
        setTimeout(() => {
          res.writeHead(200, { "content-type": "application/json" });
          res.end(
            JSON.stringify({
              id: "modr-tail",
              model: "omni-moderation-latest",
              results: [{ flagged: false, categories: {}, category_scores: {} }],
            }),
          );
        }, MODERATION_DELAY_MS);
      });
    }),
  );
}

describe("a streamed passthrough tail never carries a heartbeat inside a frame", () => {
  let app: SpawnedApp | undefined;
  let upstream: Mock | undefined;
  let moderation: Mock | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp({
      extra: { downstream: { sse_keepalive_interval_secs: HEARTBEAT_INTERVAL_S } },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    upstream = await startTailUpstream();
    moderation = await startSlowModeration();

    const pk = await seed.createProviderKey({
      display_name: "pt-tail-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createGuardrail({
      name: "pt-tail-slow-monitor",
      enabled: true,
      hook_point: "output",
      kind: "openai_moderation",
      api_key: "sk-moderation-key",
      endpoint: moderation.baseUrl,
      enforcement_mode: "monitor",
    });
    await seed.createPassthroughRoute({
      name: "pt-tail-tunnel",
      path_prefix: "/pt-tail",
      target_url: upstream.baseUrl,
      provider_key_id: pk.id,
      protocol: "openai_chat",
    });
    // key 最后写：它能鉴权时，按修订号顺序前面的资源都已生效。
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["*"],
      allowed_routes: ["*"],
    });
    await waitConfigPropagation(async () => {
      try {
        const r = await call();
        await r.text();
        return r.status === 200;
      } catch {
        return false;
      }
    }, 60_000);
  }, 90_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await moderation?.close();
  });

  function call(): Promise<Response> {
    return fetch(`${app!.proxyUrl}/pt-tail/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ model: "gpt-4o", stream: true, messages: [] }),
    });
  }

  test("heartbeats sit between frames and every data line stays valid JSON", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await call();
    expect(res.status).toBe(200);
    const body = await res.text();
    const lines = body.split("\n");

    // 审核要等 2.5s、心跳间隔 1s：流尾那段等待里必然有心跳。
    expect(
      lines.filter((l) => l === ":").length,
      `the slow end-of-stream scan should have produced a heartbeat:\n${body}`,
    ).toBeGreaterThan(0);
    for (const line of lines.filter((l) => l.startsWith("data:"))) {
      const payload = line.slice("data:".length).trim();
      expect(() => JSON.parse(payload), `a data line was corrupted: ${JSON.stringify(line)}`).not.toThrow();
    }
  }, 30_000);
});
