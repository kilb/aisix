import { createServer, type Server } from "node:net";
import { describe, expect, test } from "vitest";
import { isAddrInUseStartupFailure } from "./app.js";
import { pickFreePort, pickFreePorts, rangeBase, rangeSlotFor } from "./ports.js";

function listen(port: number): Promise<Server> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(port, "127.0.0.1", () => resolve(srv));
  });
}

function close(srv: Server): Promise<void> {
  return new Promise((resolve, reject) =>
    srv.close((err) => (err ? reject(err) : resolve())),
  );
}

describe("harness port allocator", () => {
  test("hands out distinct, immediately bindable ports from the fork's range", async () => {
    const ports = await pickFreePorts(10);
    expect(new Set(ports).size).toBe(ports.length);
    for (const p of ports) {
      expect(p).toBeGreaterThanOrEqual(13000);
      expect(p).toBeLessThan(32200);
      const srv = await listen(p); // must not be handed out already bound
      await close(srv);
    }
  });

  test("skips a port that is currently bound", async () => {
    // The cursor is monotonic, so the next pick is deterministic within
    // this process: occupy it and assert the allocator steps over it.
    const probe = await pickFreePort();
    const next = probe + 1;
    const squatter = await listen(next);
    try {
      const picked = await pickFreePort();
      expect(picked).not.toBe(next);
    } finally {
      await close(squatter);
    }
  });
});

describe("isAddrInUseStartupFailure", () => {
  const tail = "  binary state: aisix exited early with code=1 signal=null\n  stderr:\n";

  test("matches the OS error text and Rust's ErrorKind rendering", () => {
    expect(
      isAddrInUseStartupFailure(
        new Error(`${tail}Error: failed to bind proxy listener: Address already in use (os error 98)`),
      ),
    ).toBe(true);
    expect(
      isAddrInUseStartupFailure(new Error(`${tail}Error: AddrInUse binding 127.0.0.1:21001`)),
    ).toBe(true);
  });

  test("does not match other startup failures or a still-running binary", () => {
    expect(
      isAddrInUseStartupFailure(
        new Error(`${tail}Error: etcd initial sync failed: deadline exceeded`),
      ),
    ).toBe(false);
    expect(
      isAddrInUseStartupFailure(
        new Error(
          "timed out waiting for /livez\n  binary state: still running\n  stderr:\nAddress already in use",
        ),
      ),
    ).toBe(false);
  });
});

test("an e2e port slot offsets the fork's port range", () => {
  expect(rangeSlotFor(1, undefined, 99)).toBe(1);
  expect(rangeSlotFor(1, "", 99)).toBe(1);
  expect(rangeSlotFor(1, "3", 99)).toBe(4);
  expect(rangeSlotFor(13, "2", 99)).toBe(15);
  expect(rangeSlotFor(undefined, "2", 29)).toBe(7); // 29 % 24 = 5, + 2
  for (const bad of ["x", "-1", "24", "1.5"]) expect(() => rangeSlotFor(1, bad, 99)).toThrow("E2E_PORT_SLOT");
});

test("slots 14 to 23 sit below the first fourteen ranges", () => {
  expect(rangeBase(0)).toBe(21000);
  expect(rangeBase(13)).toBe(31400);
  expect(rangeBase(14)).toBe(20200);
  expect(rangeBase(23)).toBe(13000);
  expect(rangeSlotFor(1, "21", 99)).toBe(22);
  expect(() => rangeSlotFor(1, "24", 99)).toThrow("from 0 to 23");
});

test("slots 0 to 13 keep their original ranges and no range overlaps", () => {
  for (let s = 0; s <= 13; s++) expect(rangeBase(s)).toBe(21000 + s * 800);
  const bases = Array.from({ length: 24 }, (_, s) => rangeBase(s)).sort((a, b) => a - b);
  for (let i = 1; i < bases.length; i++) expect(bases[i] - bases[i - 1]).toBeGreaterThanOrEqual(800);
  expect(bases[0]).toBe(13000);
  expect(bases[23] + 799).toBeLessThan(32768);
});
