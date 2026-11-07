import { execFile } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { describe, expect, test } from "vitest";
import { stringify as yamlStringify } from "yaml";

const execFileP = promisify(execFile);

const BIN_PATH =
  process.env.AISIX_BIN ?? join(process.cwd(), "..", "..", "target", "debug", "aisix");

// What a startup config does at the boot itself, seen from outside the process.
//
// A setting that cannot work stops the boot and says why, where the operator is
// looking; one that was valid when it was written keeps booting. A gateway that
// gets past its checks serves until this test stops it, and then exits 0.

const STILL_RUNNING_AFTER_MS = 3_000;

const RESOURCES = '_format_version: "1"\n';

/** Boot with `extra` merged into a minimal file-mode config; return exit code and output. */
async function boot(
  extra: Record<string, unknown>,
): Promise<{ code: number | null; logs: string }> {
  const dir = await mkdtemp(join(tmpdir(), "aisix-boot-checks-"));
  try {
    const cfgPath = join(dir, "config.yaml");
    const resourcesFile = join(dir, "resources.yaml");
    await writeFile(resourcesFile, RESOURCES, "utf8");
    const config = {
      resources_file: resourcesFile,
      proxy: { addr: "127.0.0.1:0" },
      admin: { enabled: false },
      observability: { metrics: { prometheus: { enabled: false } } },
      shutdown: { min_drain_secs: 0 },
      ...extra,
    };
    await writeFile(cfgPath, yamlStringify(config), "utf8");
    const env: Record<string, string> = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (v !== undefined && !k.startsWith("AISIX_")) env[k] = v;
    }
    try {
      const ok = await execFileP(BIN_PATH, ["--config", cfgPath], {
        env,
        timeout: STILL_RUNNING_AFTER_MS,
      });
      return { code: 0, logs: `${ok.stdout}${ok.stderr}` };
    } catch (e) {
      const err = e as Error & { code?: number | string; stdout?: string; stderr?: string };
      return {
        code: typeof err.code === "number" ? err.code : null,
        logs: `${err.stdout ?? ""}${err.stderr ?? ""}`,
      };
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

async function withOccupiedPort<T>(run: (port: number) => Promise<T>): Promise<T> {
  const holder = createServer();
  await new Promise<void>((resolve) => holder.listen(0, "127.0.0.1", resolve));
  try {
    return await run((holder.address() as AddressInfo).port);
  } finally {
    await new Promise<void>((resolve) => holder.close(() => resolve()));
  }
}

describe("the admin listener at boot", () => {
  const admin = (addr: string, extra: Record<string, unknown> = {}) => ({
    admin: { enabled: true, addr, admin_keys: ["boot-checks-admin-key"], ...extra },
  });

  test("an occupied port stops the boot and is named", async () => {
    await withOccupiedPort(async (port) => {
      const { code, logs } = await boot(admin(`127.0.0.1:${port}`));
      expect(code, logs).not.toBe(0);
      expect(logs).toContain(`admin listener bind 127.0.0.1:${port}`);
    });
  });

  test("a certificate that cannot be loaded stops the boot and names the block", async () => {
    const { code, logs } = await boot(
      admin("127.0.0.1:0", {
        tls: { cert_file: "/nonexistent/admin.crt", key_file: "/nonexistent/admin.key" },
      }),
    );
    expect(code, logs).not.toBe(0);
    expect(logs).toContain("admin.tls");
  });
});

describe("an etcd login", () => {
  test.each([
    ["only a user", { user: "root" }],
    ["only a password variable", { password_env: "BOOT_CHECKS_ETCD_PASSWORD" }],
  ])("with %s stops the boot", async (_name, login) => {
    const { code, logs } = await boot({
      resources_file: undefined,
      etcd: { endpoints: ["http://127.0.0.1:1"], prefix: "/aisix", ...login },
    });
    expect(code, logs).not.toBe(0);
    expect(logs).toContain("etcd.user");
  });
});

describe("a request body limit of null", () => {
  test("still boots, as it did when it was written", async () => {
    const { code, logs } = await boot({
      proxy: { addr: "127.0.0.1:0", request_body_limit_bytes: null },
    });
    // Up until the test stopped it, and with nothing said about the field.
    expect(code, logs).toBe(0);
    expect(logs).not.toContain("request_body_limit_bytes");
  });
});

