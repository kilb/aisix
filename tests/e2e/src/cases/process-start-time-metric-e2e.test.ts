import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { EtcdClient, spawnApp, type SpawnedApp } from "../harness/index.js";

// E2E：指标端口暴露 `process_start_time_seconds`（Prometheus 客户端的标准名），
// 值是网关进程的启动时刻。
//
// 计数器在进程重启后从 0 重新累计，而只看计数器的值，有一种重启是认不出来的：
// 重启后、第一次抓取之前花得比重启前还多，值看起来只是涨了。按计数器计费的一方
// （门户）因此会少扣重启前那一段。启动时刻变了就是重启过，与值无关。

describe("the metrics listener exposes the process start time", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  let spawnedAfter = 0;
  let readyBy = 0;

  beforeAll(async () => {
    etcdReachable = await new EtcdClient().ping();
    if (!etcdReachable) return;
    spawnedAfter = Date.now() / 1000;
    app = await spawnApp();
    readyBy = Date.now() / 1000;
  });

  afterAll(async () => {
    await app?.exit();
  });

  test("process_start_time_seconds is the moment this gateway process started", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const body = await fetch(`${app.metricsUrl}/metrics`).then((r) => r.text());
    const line = body.split("\n").find((l) => /^process_start_time_seconds(\{[^}]*\})?\s/.test(l));
    expect(line, "process_start_time_seconds is missing from the scrape").toBeDefined();
    const value = Number(line!.trim().split(/\s+/).at(-1));
    // 进程在 spawnApp 开始之后、就绪之前启动；留一秒给时钟取整。
    expect(value).toBeGreaterThanOrEqual(Math.floor(spawnedAfter) - 1);
    expect(value).toBeLessThanOrEqual(Math.ceil(readyBy) + 1);
  });
});
