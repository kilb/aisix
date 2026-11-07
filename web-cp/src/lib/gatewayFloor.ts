import type { Instance } from "./api";

/**
 * Which gateway instances hold back a write the control plane refused with
 * `422 gateway_too_old`. The control plane judges every instance heard from
 * within the last 24 hours, online or not, by the version it last reported:
 * an unstamped build (`dev`, `dev+…`) counts as current, a release below the
 * minimum blocks, and so does a version that does not read as a release.
 */

const FLEET_WINDOW_MS = 24 * 60 * 60 * 1000;

interface Version {
  core: [number, number, number];
  pre: string[];
}

const NUM = "(0|[1-9]\\d*)";
const SEMVER = new RegExp(`^${NUM}\\.${NUM}\\.${NUM}(?:-([0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*))?(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?$`);

/** A release version, its build metadata dropped; null for anything else. */
export function parseVersion(raw: string): Version | null {
  const m = SEMVER.exec(raw);
  if (!m) return null;
  return { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ? m[4].split(".") : [] };
}

/** Whether `a` comes before `b` by semver precedence. */
export function olderThan(a: Version, b: Version): boolean {
  for (const [x, y] of a.core.map((n, i) => [n, b.core[i] ?? 0] as const)) if (x !== y) return x < y;
  // A pre-release comes before its release.
  if (a.pre.length === 0 || b.pre.length === 0) return a.pre.length > b.pre.length;
  for (const [x, y] of a.pre.map((id, i) => [id, b.pre[i]] as const)) {
    // Equal so far and `b` has run out: `a`, the longer, is newer.
    if (y === undefined) return false;
    if (x === y) continue;
    const xNum = /^\d+$/.test(x);
    const yNum = /^\d+$/.test(y);
    if (xNum && yNum) return Number(x) < Number(y);
    // Numeric identifiers come before alphanumeric ones.
    if (xNum !== yNum) return xNum;
    return x < y;
  }
  return a.pre.length < b.pre.length;
}

/** The minimum version a refusal's detail names ("需要网关版本 ≥ 1.9.0，…"), or null. */
export function minimumIn(message: string): string | null {
  return /≥\s*([0-9][0-9A-Za-z.+-]*)/.exec(message)?.[1] ?? null;
}

/**
 * The instances among `instances` that a refusal naming `minimums` is about,
 * as of `now` (ms). With no minimum that reads as a version, only the
 * instances whose version cannot be read are named.
 */
export function blockingInstances(instances: Instance[], minimums: string[], now: number): Instance[] {
  const floors = minimums.map(parseVersion).filter((v): v is Version => v !== null);
  return instances.filter((i) => {
    if (now - Date.parse(i.last_seen_at) > FLEET_WINDOW_MS) return false;
    if (i.version === "dev" || i.version.startsWith("dev+")) return false;
    const v = parseVersion(i.version);
    return v === null || floors.some((floor) => olderThan(v, floor));
  });
}
