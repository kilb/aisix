import assert from "node:assert/strict";
import { test } from "node:test";
import { adminRouteOf, canSeeAdminTab, adminHash } from "./navigation.ts";

test("旧公共模型与计费调用深链迁移并保留筛选", () => {
  const route = adminRouteOf("#pricing?section=public-models&model_id=m%2Fa&from=2026-01-01");
  assert.equal(route.tab, "public-routes");
  assert.equal(route.query.get("model_id"), "m/a");
  assert.equal(route.query.get("from"), "2026-01-01");
  assert.equal(route.query.has("section"), false);
  assert.equal(adminRouteOf("#billing?section=charges&tenant_id=t").tab, "calls");
  assert.equal(adminRouteOf("#billing?section=topups").tab, "billing");
  assert.equal(adminRouteOf("#billing").tab, "billing");
});

test("模型测试历史允许只读角色访问，账号管理仍受限", () => {
  for (const role of ["admin", "operator", "viewer"]) assert.equal(canSeeAdminTab("model-tests", role), true);
  assert.equal(canSeeAdminTab("operators", "viewer"), false);
  assert.equal(canSeeAdminTab("operators", "admin"), true);
  assert.equal(adminRouteOf("#unknown").tab, "overview");
});

test("程序导航与旧链接使用同一迁移规则", () => {
  assert.equal(adminHash("billing", "charges"), "#calls");
  assert.equal(adminHash("pricing", "public-models"), "#public-routes");
  assert.equal(adminHash("model-tests", "history"), "#model-tests?section=history");
});
