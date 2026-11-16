import { createServer, type Server, type Socket } from "node:net";
import { afterEach, describe, expect, test } from "vitest";
import { probeRedis, redisReady } from "./redis.js";

/**
 * A Redis suite skipped on CI contributes no assertions while the leg
 * reports green, so the gate has to tell a dead server, a server of the
 * wrong capability and one that proves nothing apart — and refuse to skip
 * on CI for any of them.
 */
describe("redis capability gate", () => {
  let server: Server | undefined;

  afterEach(async () => {
    delete process.env.CI;
    if (server) {
      await new Promise<void>((r) => server!.close(() => r()));
      server = undefined;
    }
  });

  /** A RESP stand-in answering each command with `reply(command)`. */
  async function fakeRedis(reply: (command: string, socket: Socket) => string): Promise<string> {
    server = createServer((sock: Socket) => {
      sock.on("data", (buf) => sock.write(reply(buf.toString().trim(), sock)));
    });
    const port = await new Promise<number>((r) =>
      server!.listen(0, "127.0.0.1", () => {
        const a = server!.address();
        r(typeof a === "object" && a ? a.port : 0);
      }),
    );
    return `redis://127.0.0.1:${port}`;
  }

  const vector = (c: string) => (c === "PING" ? "+PONG\r\n" : "*0\r\n");
  const plain = (c: string) =>
    c === "PING" ? "+PONG\r\n" : "-ERR unknown command 'FT._LIST', with args beginning with:\r\n";
  const locked = () => "-NOAUTH Authentication required.\r\n";

  test("each capability is told apart", async () => {
    expect(await probeRedis(await fakeRedis(vector))).toBe("vector");
    await new Promise<void>((r) => server!.close(() => r()));
    expect(await probeRedis(await fakeRedis(plain))).toBe("plain");
    await new Promise<void>((r) => server!.close(() => r()));
    expect(await probeRedis(await fakeRedis(locked))).toBe("unknown");
    expect(await probeRedis("redis://127.0.0.1:1")).toBe("unreachable");
    expect(await probeRedis("rediss://127.0.0.1:6379")).toBe("unknown");
  });

  test("off CI a mismatch skips", async () => {
    delete process.env.CI;
    const url = await fakeRedis(plain);
    expect(await redisReady(url, "any")).toBe(true);
    expect(await redisReady(url, "plain")).toBe(true);
    expect(await redisReady(url, "vector")).toBe(false);
    expect(await redisReady("redis://127.0.0.1:1", "any")).toBe(false);
  });

  test("on CI a dead, unknown or wrong-capability redis fails the run", async () => {
    process.env.CI = "true";
    await expect(redisReady("redis://127.0.0.1:1", "any")).rejects.toThrow(/unreachable/);
    const plainUrl = await fakeRedis(plain);
    await expect(redisReady(plainUrl, "vector")).rejects.toThrow(/plain/);
    expect(await redisReady(plainUrl, "plain")).toBe(true);
    await new Promise<void>((r) => server!.close(() => r()));
    const vectorUrl = await fakeRedis(vector);
    await expect(redisReady(vectorUrl, "plain")).rejects.toThrow(/vector/);
    expect(await redisReady(vectorUrl, "vector")).toBe(true);
    await new Promise<void>((r) => server!.close(() => r()));
    await expect(redisReady(await fakeRedis(locked), "any")).rejects.toThrow(/unknown/);
  });

  // 恢复原始 URL 插值应让凭据 canary 断言变红，错误仍须保留可定位的地址。
  test("CI failures hide URL credentials, path and query", async () => {
    process.env.CI = "true";
    const url = await fakeRedis(locked);
    const configured = url.replace("redis://", "redis://probe-user:redis-credential-canary%40tail@") +
      "/9?token=redis-query-canary#redis-fragment-canary";
    const failure = await redisReady(configured, "any").catch((error: Error) => error);
    expect(failure).toBeInstanceOf(Error);
    const text = String(failure);
    for (const secret of ["redis-credential-canary", "probe-user", "tail", "redis-query-canary", "redis-fragment-canary"]) {
      expect(text).not.toContain(secret);
    }
    expect(text).toContain(url + "/");
    expect(text).toContain("unknown");
  });

  // PING 和 FT._LIST 各开一条连接，任一连接漏 AUTH 都不能判为 plain。
  test.each([
    [":password-canary", "*2\r\n$4\r\nAUTH\r\n$15\r\npassword-canary"],
    ["probe:p%20%40%0D%0A%E5%AF%86%E7%A0%81", "*3\r\n$4\r\nAUTH\r\n$5\r\nprobe\r\n$11\r\np @\r\n密码"],
  ])("each connection authenticates with URL userinfo %s", async (userinfo, auth) => {
    const authenticated = new WeakSet<Socket>();
    const url = await fakeRedis((command, socket) => {
      if (command === auth) {
        authenticated.add(socket);
        return "+OK\r\n";
      }
      return authenticated.has(socket) ? plain(command) : locked();
    });
    expect(await probeRedis(url.replace("redis://", `redis://${userinfo}@`))).toBe("plain");
  });

  test.each(["PING", "FT._LIST"])("retries LOADING from %s within the existing attempt limit", async (loadingCommand) => {
    let calls = 0;
    const url = await fakeRedis((command) => {
      if (command === loadingCommand && ++calls < 3) return "-LOADING Redis is loading the dataset in memory\r\n";
      return plain(command);
    });
    expect(await probeRedis(url)).toBe("plain");
    expect(calls).toBe(3);
  });

  test("persistent LOADING remains unknown after three attempts", async () => {
    let calls = 0;
    const url = await fakeRedis(() => {
      calls++;
      return "-LOADING Redis is loading the dataset in memory\r\n";
    });
    expect(await probeRedis(url)).toBe("unknown");
    expect(calls).toBe(3);
  });
});
