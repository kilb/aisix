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
  async function fakeRedis(reply: (command: string) => string): Promise<string> {
    server = createServer((sock: Socket) => {
      sock.on("data", (buf) => sock.write(reply(buf.toString().trim())));
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
});
