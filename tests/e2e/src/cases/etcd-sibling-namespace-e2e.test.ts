import { randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: two gateways sharing one etcd under `<prefix>` and `<prefix>-dev`.
//
// The two namespaces have nothing to do with each other. A row written under
// the neighbour must not appear in this gateway's status at all: not as a
// rejection, which would leave its reload signal red for good over a row
// nobody here can fix, and certainly not read into its memory and its snapshot
// cache, which is where the neighbour's provider credentials would land.

describe("a neighbouring namespace on the same etcd", () => {
  let etcd: EtcdClient | undefined;
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  const neighbourId = randomUUID();

  beforeAll(async () => {
    etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    app = await spawnApp();
  });

  afterAll(async () => {
    if (app) await etcd?.deletePrefix(`${app.etcdPrefix}-dev`);
    await app?.exit();
  });

  test("leaves this gateway's status alone", async (ctx) => {
    if (!etcdReachable || !etcd || !app) {
      ctx.skip();
      return;
    }
    const status = async () =>
      (await (await fetch(`${app!.metricsUrl}/status/config`)).json()) as {
        rejected: Array<{ resource_id: string }>;
        last_reload?: { successful: boolean };
        applied?: { resource_counts: Record<string, number> };
      };

    // Written before and after the gateway's own row: the first read and the
    // watch both have to stay on this side of the boundary.
    const neighbourKey = `${app.etcdPrefix}-dev/provider_keys/${neighbourId}`;
    const neighbourDoc = JSON.stringify({
      display_name: "neighbour-pk",
      provider: "openai",
      api_key: "sk-neighbour-secret",
    });
    await etcd.put(neighbourKey, neighbourDoc);

    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createProviderKey({
      display_name: "own-pk",
      provider: "openai",
      api_key: "sk-own-secret",
    });
    await waitConfigPropagation(
      async () => (await status()).applied?.resource_counts.provider_keys === 1,
    );
    await etcd.put(`${neighbourKey}-2`, neighbourDoc);
    await seed.createProviderKey({
      display_name: "own-pk-2",
      provider: "openai",
      api_key: "sk-own-secret",
    });
    await waitConfigPropagation(
      async () => (await status()).applied?.resource_counts.provider_keys === 2,
    );

    const cfg = await status();
    expect(cfg.rejected).toEqual([]);
    expect(cfg.last_reload?.successful).toBe(true);
  });
});
