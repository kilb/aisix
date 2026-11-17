import assert from "node:assert/strict";
import { test } from "node:test";
import type { Instance } from "./api.ts";
import { blockingInstances, tokenCountReviewInstances } from "./gatewayFloor.ts";

test("fixed counter diagnostics retain old dev stamps and offline history but exclude explicitly retired instances", () => {
  const now = Date.parse("2026-11-17T12:00:00Z");
  const instance = (id: string, extra: Record<string, unknown>): Instance => ({
    instance_id: id, version: "1.0.0", last_seen_at: "2026-11-17T10:00:00Z", online: false, ...extra,
  } as Instance);
  const instances = [
    instance("offline", {}),
    instance("retired", { retired_at: "2026-11-17T11:00:00Z" }),
    instance("pre-feature-dev", { version: "dev+sha-641ccf9" }),
    instance("expired", { last_seen_at: "2026-11-15T10:00:00Z" }),
    instance("current", { version: "9.0.0", online: true }),
  ];
  assert.deepEqual(blockingInstances(instances, ["2.0.0"], now).map((i) => i.instance_id), ["offline", "retired"]);
  assert.deepEqual(tokenCountReviewInstances(instances, now).map((i) => i.instance_id), ["offline", "pre-feature-dev", "current"]);
});
