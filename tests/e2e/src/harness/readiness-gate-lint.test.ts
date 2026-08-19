import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, test } from "vitest";

/**
 * Guards the readiness-gate rule from `tests/e2e/AGENTS.md`:
 *
 *   "Seed every caller API key after every other resource, then gate on that
 *    key authenticating. That single condition implies the whole seed set is
 *    in the snapshot."
 *
 * The failure it prevents is specific and keeps recurring. A gate that polls
 * `admin.listModelStatuses()` runs on the ADMIN key, so it can go true while
 * the caller key — seeded last, at a higher etcd revision — has not
 * propagated. The request under test then returns 401 and the spec fails on
 * an assertion that has nothing to do with what it was testing. Because the
 * window only opens under load, it passes in isolation and flakes in a full
 * run, which is the worst shape a test failure can have: it reads as an
 * infrastructure hiccup and gets re-run rather than fixed.
 *
 * Four specs carried this before the 2026-08-19 pass. A lint is cheaper than
 * noticing the fifth.
 */

const CASES_DIR = join(__dirname, "..", "cases");

/** Evidence inside a gate closure that the CALLER key authenticated. */
const PROVES_CALLER_KEY = [
  "listModels",
  "ProxyClient",
  "proxyUrl",
  "postJson",
  "completions.create",
];

/** A probe that runs on the admin key and says nothing about the caller. */
const ADMIN_ONLY_PROBE = ["listModelStatuses", "admin!.", "adminUrl"];

/** Body of every `waitConfigPropagation(async () => { … })` in `src`. */
function gateBodies(src: string): string[] {
  const bodies: string[] = [];
  const opener = "waitConfigPropagation(async () => {";
  let from = 0;
  for (;;) {
    const start = src.indexOf(opener, from);
    if (start === -1) return bodies;
    let depth = 1;
    let i = start + opener.length;
    while (i < src.length && depth > 0) {
      if (src[i] === "{") depth++;
      else if (src[i] === "}") depth--;
      i++;
    }
    bodies.push(src.slice(start + opener.length, i));
    from = i;
  }
}

describe("readiness gates imply what the spec then asserts", () => {
  test("no gate waits only on an admin-key probe", () => {
    const offenders: string[] = [];

    for (const file of readdirSync(CASES_DIR).filter((f) => f.endsWith(".test.ts"))) {
      const src = readFileSync(join(CASES_DIR, file), "utf8");
      const bodies = gateBodies(src);

      // A spec is fine if some gate proves the caller key; a spec with only
      // admin-key gates is the shape that flakes.
      const anyGateProvesKey = bodies.some((b) =>
        PROVES_CALLER_KEY.some((marker) => b.includes(marker)),
      );
      const anyGateIsAdminOnly = bodies.some(
        (b) =>
          ADMIN_ONLY_PROBE.some((marker) => b.includes(marker)) &&
          !PROVES_CALLER_KEY.some((marker) => b.includes(marker)),
      );

      if (anyGateIsAdminOnly && !anyGateProvesKey) offenders.push(file);
    }

    expect(
      offenders,
      `these specs gate only on an admin-key probe, so the gate can pass while ` +
        `the caller key is still propagating and the request 401s under load:\n` +
        offenders.map((f) => `  ${f}`).join("\n") +
        `\n\nAdd a caller-key gate before the runtime probe:\n` +
        `  await waitConfigPropagation(async () => {\n` +
        `    const probe = new ProxyClient(app!.proxyUrl, CALLER_PLAINTEXT);\n` +
        `    return (await probe.listModels()).status === 200;\n` +
        `  });`,
    ).toEqual([]);
  });

  /**
   * The companion half of the rule. A caller-key gate only implies the seed
   * set if nothing is written after the key — `ratelimit-e2e` seeded its
   * policy afterwards and the gate opened before any limit existed, so the
   * spec's first assertion saw 200 where it wanted 429.
   *
   * Specs whose gate independently proves the trailing resource (waiting for a
   * guardrail to actually block, or for a router to return a specific body)
   * are exempt — the list below is that exemption, and it should only shrink.
   */
  test("nothing is seeded after the caller key", () => {
    // Gates here assert on the trailing resource's own effect, so the key
    // does not have to be last for them to be sound.
    const GATE_PROVES_TRAILING_RESOURCE = new Set([
      "batch-files-finetuning-e2e.test.ts",
      "guardrail-aliyun-ai-guardrail-e2e.test.ts",
      "guardrail-aliyun-e2e.test.ts",
      "guardrail-aliyun-request-id-e2e.test.ts",
      "guardrail-keyword-e2e.test.ts",
      "guardrail-keyword-message-locations-e2e.test.ts",
      "guardrail-lakera-e2e.test.ts",
      "guardrail-metrics-e2e.test.ts",
      "guardrail-model-scope-e2e.test.ts",
      "guardrail-monitor-mode-e2e.test.ts",
      "guardrail-monitor-provider-failure-e2e.test.ts",
      "guardrail-monitor-telemetry-e2e.test.ts",
      "model-kind-dead-knobs-e2e.test.ts",
      "provider-key-tls-e2e.test.ts",
      "responses-streaming-monitor-guardrail-e2e.test.ts",
      "semantic-routing-e2e.test.ts",
    ]);

    const offenders: string[] = [];
    for (const file of readdirSync(CASES_DIR).filter((f) => f.endsWith(".test.ts"))) {
      if (GATE_PROVES_TRAILING_RESOURCE.has(file)) continue;
      const src = readFileSync(join(CASES_DIR, file), "utf8");
      const before = /beforeAll\(async \(\) => \{([\s\S]*?)\n  \}\)/.exec(src);
      if (!before) continue;
      const body = before[1];
      const seeds = [...body.matchAll(/seed\.create\w+|seed\.update\(|etcd\.put\(/g)];
      if (seeds.length < 2) continue;
      const keyIdx = seeds
        .map((m, i) => ({ i, near: body.slice(m.index ?? 0, (m.index ?? 0) + 200) }))
        .filter((e) => e.near.includes("createApiKey") || e.near.includes("api_keys"))
        .map((e) => e.i);
      if (keyIdx.length === 0) continue;
      if (Math.max(...keyIdx) !== seeds.length - 1) offenders.push(file);
    }

    expect(
      offenders,
      `these specs write a resource AFTER the caller key, so a gate that waits ` +
        `on the key can open before that resource has propagated:\n` +
        offenders.map((f) => `  ${f}`).join("\n") +
        `\n\nMove the caller key to the end of beforeAll, or — if the gate ` +
        `already proves the trailing resource — add the file to ` +
        `GATE_PROVES_TRAILING_RESOURCE with that reason.`,
    ).toEqual([]);
  });

  test("the scan actually reaches the specs", () => {
    const files = readdirSync(CASES_DIR).filter((f) => f.endsWith(".test.ts"));
    expect(files.length).toBeGreaterThan(100);
    // And the parser finds gates, rather than silently matching nothing.
    const withGates = files.filter(
      (f) => gateBodies(readFileSync(join(CASES_DIR, f), "utf8")).length > 0,
    );
    expect(withGates.length).toBeGreaterThan(20);
  });
});
