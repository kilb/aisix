import { createServer, type Server } from "node:http";
import { afterEach, describe, expect, test } from "vitest";
import { EtcdClient } from "./etcd.js";

/**
 * Every spec gates its whole `describe` on `EtcdClient.ping()` and skips
 * silently when it answers false. That makes the probe's own reliability a
 * coverage question: one slow response erases a block's worth of tests and
 * the run still reports success, just with a larger skip count.
 *
 * Observed on this repo: two runs of the same tree reported 2 and then 3
 * skipped, and the extra one was a probe that timed out — not a test with
 * anything to say.
 */
describe("etcd reachability probe", () => {
  let server: Server | undefined;

  afterEach(async () => {
    delete process.env.CI;
    if (server) {
      await new Promise<void>((r) => server!.close(() => r()));
      server = undefined;
    }
  });

  test("a probe that fails twice and then answers is reachable", async () => {
    let attempts = 0;
    server = createServer((_req, res) => {
      attempts += 1;
      if (attempts < 3) {
        res.socket?.destroy();
        return;
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ header: { cluster_id: "1" } }));
    });
    const port = await new Promise<number>((r) =>
      server!.listen(0, "127.0.0.1", () => {
        const a = server!.address();
        r(typeof a === "object" && a ? a.port : 0);
      }),
    );

    const reachable = await new EtcdClient(`http://127.0.0.1:${port}`).ping();
    expect(reachable, "一次失败就判定不可达 —— 那会静默抹掉一整块用例").toBe(true);
    expect(attempts).toBe(3);
  });

  test("nothing listening is unreachable, and cheaply so", async () => {
    delete process.env.CI;
    const started = Date.now();
    const reachable = await new EtcdClient("http://127.0.0.1:1").ping();
    expect(reachable).toBe(false);
    // 连接被拒是立即返回的，所以重试不会让「本地没有 etcd」这个常见情形变慢。
    expect(Date.now() - started).toBeLessThan(5_000);
  });

  test("in CI an unreachable etcd fails instead of skipping the suite", async () => {
    process.env.CI = "true";
    await expect(new EtcdClient("http://127.0.0.1:1").ping()).rejects.toThrow(
      /unreachable/,
    );
  });
});
