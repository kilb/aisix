import { execFile } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { stringify as yamlStringify } from "yaml";

const execFileP = promisify(execFile);

const BIN_PATH =
  process.env.AISIX_BIN ?? join(process.cwd(), "..", "..", "target", "debug", "aisix");

// E2E: a resources file written for an earlier release, carrying fields that
// release had and this one retired.
//
// The file is refused: it is a write, and a field this release does not read
// would otherwise sit there looking configured. So what the refusal says is
// all an operator has to upgrade by, and they read it twice: from `aisix
// validate`, the check before rolling out, and from the boot that refused the
// file. Both have to name each retired field and say what to write instead,
// and they have to say the same thing.

const RESOURCES = `
_format_version: "1"
provider_keys:
  - display_name: retired-pk
    provider: openai
    api_key: sk-mock
models:
  - display_name: retired-primary
    provider: openai
    model_name: gpt-4o-mini
    provider_key: retired-pk
  - display_name: retired-backup
    provider: openai
    model_name: gpt-4o-mini
    provider_key: retired-pk
  - display_name: retired-weighted-group
    routing:
      strategy: weighted
      targets:
        - model: retired-primary
          weight: 100
        - model: retired-backup
          weight: 0
  - display_name: retired-sticky-group
    routing:
      strategy: weighted
      sticky: true
      targets:
        - model: retired-primary
api_keys:
  - display_name: retired-tools-key
    key_hash: 1460db1b6902f8b1fc2a40d9381a24d0fd22c3bc1b2c6f999c521da73776fbe0
    allowed_models: ["*"]
    allowed_tools: ["github__*"]
  - display_name: retired-mode-key
    key_hash: 2460db1b6902f8b1fc2a40d9381a24d0fd22c3bc1b2c6f999c521da73776fbe0
    allowed_models: ["*"]
    mcp_access:
      mode: inherit
guardrails:
  - name: retired-mandatory-rule
    kind: keyword
    mandatory: true
    patterns:
      - kind: literal
        value: AKIA
passthrough_routes:
  - name: retired-route
    path_prefix: /retired
    target_url: https://example.com
    provider_key: retired-pk
    protocol: openai
    streaming: true
`;

/** What each entry's refusal has to say: the retired field, and what to write. */
const EXPECTED: Array<[string, string[]]> = [
  ['"retired-weighted-group"', ["weighted", "round_robin", "weight: 0", "priority: -1"]],
  ['"retired-sticky-group"', ["sticky", "consistent_hash"]],
  ['"retired-tools-key"', ["allowed_tools", "mcp_access"]],
  ['"retired-mode-key"', ["mcp_access.mode", "allow"]],
  ['"retired-mandatory-rule"', ["mandatory", "fail_open"]],
  ['"retired-route"', ["protocol", "streaming"]],
];

async function run(args: string[]): Promise<{ code: number | null; output: string }> {
  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v !== undefined && !k.startsWith("AISIX_")) env[k] = v;
  }
  try {
    const ok = await execFileP(BIN_PATH, args, { env, timeout: 5_000 });
    return { code: 0, output: `${ok.stdout}${ok.stderr}` };
  } catch (e) {
    const err = e as Error & { code?: number | string; stdout?: string; stderr?: string };
    return {
      code: typeof err.code === "number" ? err.code : null,
      output: `${err.stdout ?? ""}${err.stderr ?? ""}`,
    };
  }
}

describe("a resources file carrying retired fields", () => {
  let dir: string;
  let resourcesFile: string;
  let configFile: string;

  beforeAll(async () => {
    dir = await mkdtemp(join(tmpdir(), "aisix-retired-fields-"));
    resourcesFile = join(dir, "resources.yaml");
    configFile = join(dir, "config.yaml");
    await writeFile(resourcesFile, RESOURCES, "utf8");
    await writeFile(
      configFile,
      yamlStringify({
        resources_file: resourcesFile,
        proxy: { addr: "127.0.0.1:0" },
        admin: { enabled: false },
        observability: { metrics: { prometheus: { enabled: false } } },
        shutdown: { min_drain_secs: 0 },
      }),
      "utf8",
    );
  });

  afterAll(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  /** The line about one entry, or a failure naming what was printed instead. */
  function lineFor(output: string, entry: string): string {
    const line = output.split("\n").find((l) => l.includes(entry));
    expect(line, `no line about ${entry} in:\n${output}`).toBeDefined();
    return line!;
  }

  test("`aisix validate` names each one and what to write instead", async () => {
    const { code, output } = await run(["validate", "--resources", resourcesFile]);
    expect(code, output).not.toBe(0);
    for (const [entry, parts] of EXPECTED) {
      const line = lineFor(output, entry);
      for (const part of parts) expect(line, entry).toContain(part);
    }
  });

  test("the boot that refuses the file says the same", async () => {
    const validated = await run(["validate", "--resources", resourcesFile]);
    const booted = await run(["--config", configFile]);
    expect(booted.code, booted.output).not.toBe(0);
    expect(booted.code, "the gateway should not still be up").not.toBeNull();
    for (const [entry] of EXPECTED) {
      expect(booted.output).toContain(lineFor(validated.output, entry).trim());
    }
  });
});
