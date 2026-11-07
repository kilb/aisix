import { spawnSync } from "node:child_process";
import { accessSync, constants } from "node:fs";

/**
 * Fail the control-plane run before any case starts when what every case
 * needs is missing: a Postgres the stack can create databases on
 * (`CP_E2E_PG_URL`) and a kine binary (`CP_E2E_KINE_BIN`, else `kine` on
 * PATH). Each case would otherwise fail in its own `beforeAll`, after a
 * timeout, with the cause buried in the stack's output.
 *
 * Locally, `scripts/cp-test-services.sh start` and then
 * `eval "$(scripts/cp-test-services.sh env)"` provide both.
 */
export function setup(): void {
  const pgUrl = process.env.CP_E2E_PG_URL;
  if (!pgUrl) {
    throw new Error(
      "CP_E2E_PG_URL is not set: the control-plane cases need a Postgres whose role may CREATE " +
        'DATABASE. Locally: scripts/cp-test-services.sh start && eval "$(scripts/cp-test-services.sh env)"',
    );
  }
  const psql = process.env.CP_E2E_PSQL || "psql";
  const probe = spawnSync(psql, [pgUrl, "-qAtc", "SELECT 1"], { encoding: "utf8", timeout: 15_000 });
  if (probe.error || probe.status !== 0 || probe.stdout.trim() !== "1") {
    throw new Error(
      `Postgres is not reachable through CP_E2E_PG_URL (${psql}: ` +
        `${probe.error?.message || probe.stderr?.trim() || `exit ${probe.status}`})`,
    );
  }

  const kine = process.env.CP_E2E_KINE_BIN;
  if (kine) {
    try {
      accessSync(kine, constants.X_OK);
    } catch {
      throw new Error(`CP_E2E_KINE_BIN (${kine}) is not an executable file`);
    }
  } else if (spawnSync("kine", ["--version"], { encoding: "utf8" }).error) {
    throw new Error(
      "kine is missing: set CP_E2E_KINE_BIN or put kine on PATH (scripts/fetch-kine.sh fetches the pinned binary)",
    );
  }
}
