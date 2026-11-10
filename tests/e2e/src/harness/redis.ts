import { connect } from "node:net";
import { onCI } from "./etcd.js";

/**
 * What a Redis endpoint turned out to be:
 *
 * - `vector`: answers `PING` and speaks the vector-search command family
 *   (`FT._LIST` returns an array) — Redis 8 / Redis Stack.
 * - `plain`: answers `PING` and refuses `FT._LIST` as an unknown command.
 * - `unknown`: reachable, but nothing proves either way (a `rediss://` URL
 *   this plaintext probe cannot speak, auth required, still loading).
 * - `unreachable`: no `PONG` after every retry.
 */
export type RedisCapability = "vector" | "plain" | "unknown" | "unreachable";

/** What a suite needs: any reachable Redis, or one of each capability. */
export type RedisNeed = "any" | "vector" | "plain";

const ATTEMPTS = 3;

function hostPort(url: string): { host: string; port: number } | null {
  const m = /^redis:\/\/(?:[^@/]*@)?([^:/]+)(?::(\d+))?/.exec(url);
  if (!m) return null;
  return { host: m[1], port: m[2] ? Number(m[2]) : 6379 };
}

/** The first reply chunk to one RESP command, or `null` on error/timeout. */
function firstReply(url: string, command: string, timeoutMs: number): Promise<string | null> {
  const target = hostPort(url);
  if (!target) return Promise.resolve(null);
  return new Promise((resolve) => {
    const sock = connect(target, () => sock.write(`${command}\r\n`));
    const done = (v: string | null) => {
      sock.destroy();
      resolve(v);
    };
    sock.once("data", (buf) => done(buf.toString()));
    sock.once("error", () => done(null));
    sock.setTimeout(timeoutMs, () => done(null));
  });
}

/**
 * Probe `url`, retrying before calling it unreachable: a container still
 * opening its listener refuses the connection in milliseconds, so only a
 * retry spans a cold start — the same reason `EtcdClient.ping` retries.
 */
export async function probeRedis(url: string): Promise<RedisCapability> {
  // Plaintext RESP probe: a rediss:// target cannot be probed this way —
  // that is "unknown", not "unsupported".
  if (/^rediss:\/\//.test(url) || !hostPort(url)) return "unknown";
  for (let attempt = 1; attempt <= ATTEMPTS; attempt++) {
    const pong = await firstReply(url, "PING", 1000 * attempt);
    if (pong === null) {
      if (attempt < ATTEMPTS) await new Promise((r) => setTimeout(r, 500 * attempt));
      continue;
    }
    if (!pong.startsWith("+PONG")) return "unknown";
    const ft = await firstReply(url, "FT._LIST", 1000 * attempt);
    if (ft?.startsWith("*")) return "vector";
    // Only an explicit unknown-command error PROVES the capability is
    // absent; anything else proves nothing either way.
    if (ft !== null && /^-ERR unknown command/i.test(ft)) return "plain";
    return "unknown";
  }
  return "unreachable";
}

function satisfies(found: RedisCapability, need: RedisNeed): boolean {
  if (need === "any") return found === "vector" || found === "plain";
  return found === need;
}

/**
 * Whether the Redis at `url` is what a suite needs, so the suite can run.
 *
 * Off CI the answer may be `false` and the suite skips: a developer without
 * a Redis, or without the right one, is a legitimate local run. On CI it
 * throws instead, because CI provisions a vector-capable Redis on
 * `AISIX_E2E_REDIS` (6379) and a plain one on `AISIX_E2E_REDIS_PLAIN`
 * (6377): a suite that skips there contributes no assertions while the leg
 * still reports green, so a dead or wrong-capability service would remove
 * the coverage silently.
 */
export async function redisReady(url: string, need: RedisNeed): Promise<boolean> {
  const found = await probeRedis(url);
  if (satisfies(found, need)) return true;
  if (onCI()) {
    throw new Error(
      `redis at ${url} is ${found}, but this suite needs ${need === "any" ? "a reachable redis" : `a ${need} redis`}. ` +
        "Skipping here would drop the suite and still pass the run. Check the redis services in " +
        ".github/workflows/ci.yml against AISIX_E2E_REDIS / AISIX_E2E_REDIS_PLAIN.",
    );
  }
  return false;
}
