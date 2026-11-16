import assert from "node:assert/strict";
import test from "node:test";

// 去掉禁用映射、把无映射当未知或把已退役模型计入时必须变红。
test("账号映射数来自未退役公共模型的已保存映射，禁用映射仍计入", async () => {
  const helpers = await import("./providerMappings.ts");
  const counts = helpers.providerMappingCounts([
    { id: "public-a", retired_at: null, served_by: [{ provider_key_id: "a", enabled: true }, { provider_key_id: "b", enabled: false }] },
    { id: "public-b", retired_at: null, served_by: [{ provider_key_id: "a", enabled: false }] },
    { id: "retired", retired_at: "2026-11-10T00:00:00Z", served_by: [{ provider_key_id: "c", enabled: true }] },
  ]);
  assert.equal(counts.get("a"), 2);
  assert.equal(counts.get("b"), 1);
  assert.equal(counts.get("c") ?? 0, 0);
  assert.equal(counts.get("no-mapping") ?? 0, 0);
});
