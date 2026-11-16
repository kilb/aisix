import { connect } from "node:net";
import { onCI } from "./etcd.js";
import { redactUrlForLog } from "./url-redact.js";

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

interface RedisTarget {
  host: string;
  port: number;
  auth?: string[];
}

function redisTarget(raw: string): RedisTarget | null {
  try {
    const url = new URL(raw);
    if (url.protocol !== "redis:" || !url.hostname) return null;
    const username = decodeURIComponent(url.username);
    const password = decodeURIComponent(url.password);
    return {
      host: url.hostname.replace(/^\[|\]$/g, ""),
      port: url.port ? Number(url.port) : 6379,
      auth: username ? ["AUTH", username, password] : password ? ["AUTH", password] : undefined,
    };
  } catch (error) {
    if (error instanceof TypeError || error instanceof URIError) return null;
    throw error;
  }
}

/** The first RESP reply line, or `null` on error/timeout. */
function firstReply(target: RedisTarget, command: string, deadline: number): Promise<string | null> {
  return new Promise((resolve) => {
    let authenticated = !target.auth;
    let reply = "";
    const sock = connect({ host: target.host, port: target.port }, () => {
      // RESP 的字节长度保留空格、CRLF 与非 ASCII 密码，不把凭据拼成内联命令。
      const auth = target.auth;
      sock.write(auth
        ? `*${auth.length}\r\n${auth.map((part) => `$${Buffer.byteLength(part)}\r\n${part}\r\n`).join("")}`
        : `${command}\r\n`);
    });
    const done = (v: string | null) => {
      clearTimeout(timer);
      sock.destroy();
      resolve(v);
    };
    // AUTH 和实际命令共用绝对期限，零碎响应不能刷新探测预算。
    const timer = setTimeout(() => done(null), Math.max(0, deadline - Date.now()));
    sock.on("data", (buf) => {
      reply += buf.toString();
      const end = reply.indexOf("\r\n");
      if (end < 0) return;
      const line = reply.slice(0, end + 2);
      if (authenticated || line !== "+OK\r\n") return done(line);
      authenticated = true;
      reply = "";
      sock.write(`${command}\r\n`);
    });
    sock.once("error", () => done(null));
    sock.once("end", () => done(null));
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
  const target = redisTarget(url);
  if (!target) return "unknown";
  let found: RedisCapability = "unreachable";
  for (let attempt = 1; attempt <= ATTEMPTS; attempt++) {
    // 每次原有的 1/2/3 秒预算覆盖连接、AUTH、PING 与 FT._LIST，不增加重试次数。
    const deadline = Date.now() + 1000 * attempt;
    const pong = await firstReply(target, "PING", deadline);
    if (pong !== null) {
      found = "unknown";
      if (pong.startsWith("+PONG")) {
        const ft = await firstReply(target, "FT._LIST", deadline);
        if (ft?.startsWith("*")) return "vector";
        // 只有明确的未知命令才能证明不支持向量搜索；LOADING 仍需在原预算内重试。
        if (ft !== null && /^-ERR unknown command/i.test(ft)) return "plain";
        if (!ft?.startsWith("-LOADING")) return "unknown";
      } else if (!pong.startsWith("-LOADING")) {
        return "unknown";
      }
    }
    if (attempt < ATTEMPTS) await new Promise((r) => setTimeout(r, 500 * attempt));
  }
  return found;
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
      `redis at ${redactUrlForLog(url)} is ${found}, but this suite needs ${need === "any" ? "a reachable redis" : `a ${need} redis`}. ` +
        "Skipping here would drop the suite and still pass the run. Check the redis services in " +
        ".github/workflows/ci.yml against AISIX_E2E_REDIS / AISIX_E2E_REDIS_PLAIN.",
    );
  }
  return false;
}
