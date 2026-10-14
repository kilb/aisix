import net from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { EtcdClient, spawnApp, type SpawnedApp } from "../harness/index.js";

// E2E：网关在请求体还没传完时就拒掉请求（这里是鉴权失败），调用方必须能收到
// 那个拒绝响应。
//
// 拒绝本身可以立即发出，但连接不能随即关掉：调用方这时往往还在写请求体，
// 写一个已经被对端关掉的连接得到的是 `EPIPE` / 连接重置，它就永远看不到
// 401，也不知道是 key 错了，SDK 会把它当成网络故障去重试。这里用原始 socket
// 精确控制「请求头先到、请求体后到」的时序，而不是赌一次竞争。

const BODY_DELAY_MS = 200;

interface Outcome {
  statusLine: string;
  /** 请求体还没发出去，连接就已经被网关关了。 */
  closedBeforeBody: boolean;
  writeError: string | null;
}

function upload(port: number, framing: "content-length" | "chunked"): Promise<Outcome> {
  const body = "x".repeat(1000);
  const head =
    "POST /v1/files HTTP/1.1\r\nHost: gateway\r\n" +
    "Authorization: Bearer sk-not-a-real-key\r\n" +
    "Content-Type: application/octet-stream\r\n" +
    (framing === "content-length"
      ? `Content-Length: ${body.length}\r\n\r\n`
      : "Transfer-Encoding: chunked\r\n\r\n");
  const payload =
    framing === "content-length" ? body : `${body.length.toString(16)}\r\n${body}\r\n0\r\n\r\n`;

  return new Promise((resolve) => {
    const sock = net.connect(port, "127.0.0.1");
    let response = "";
    let closed = false;
    let bodyStepDone = false;
    let closedBeforeBody = false;
    let writeError: string | null = null;
    // 两件事都落定才出结果：请求体那一步已经走过（发出或因连接已关而放弃），
    // 并且连接已经关闭。
    const maybeFinish = () => {
      if (closed && bodyStepDone) {
        resolve({ statusLine: response.split("\r\n")[0] ?? "", closedBeforeBody, writeError });
      }
    };
    sock.on("data", (d) => (response += d.toString()));
    sock.on("error", (e: NodeJS.ErrnoException) => (writeError = e.code ?? e.message));
    sock.on("close", () => {
      closed = true;
      maybeFinish();
    });
    sock.write(head);
    setTimeout(() => {
      if (closed || sock.destroyed) {
        closedBeforeBody = true;
        bodyStepDone = true;
        maybeFinish();
        return;
      }
      sock.write(payload, (err) => {
        if (err) writeError = (err as NodeJS.ErrnoException).code ?? err.message;
        bodyStepDone = true;
        // 请求体已送出：给网关一点时间回完响应，然后由我们这边收尾。
        setTimeout(() => sock.end(), 200);
        maybeFinish();
      });
    }, BODY_DELAY_MS);
  });
}

describe("a request rejected while its body is still uploading gets the rejection", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    etcdReachable = await new EtcdClient().ping();
    if (!etcdReachable) return;
    app = await spawnApp();
  });

  afterAll(async () => {
    await app?.exit();
  });

  for (const framing of ["content-length", "chunked"] as const) {
    test(`${framing} body: the 401 arrives and the connection outlives the upload`, async (ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      const port = Number(new URL(app.proxyUrl).port);
      const outcome = await upload(port, framing);
      expect(outcome.statusLine).toContain(" 401 ");
      expect(
        outcome.closedBeforeBody,
        "the gateway closed the connection while the caller was still uploading",
      ).toBe(false);
      expect(outcome.writeError).toBeNull();
    });
  }
});
