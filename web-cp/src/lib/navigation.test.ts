import assert from "node:assert/strict";
import { test } from "node:test";
import { adminRouteOf, canSeeAdminTab, adminHash, tenantHash, tenantViewOf } from "./navigation.ts";

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

test("上游监测有可分享入口且允许只读运维角色访问", () => {
  assert.equal(adminRouteOf("#upstream-monitoring?model_id=direct-a").tab, "upstream-monitoring");
  assert.equal(adminHash("upstream-monitoring"), "#upstream-monitoring");
  for (const role of ["admin", "operator", "viewer"]) assert.equal(canSeeAdminTab("upstream-monitoring", role), true);
});

test("总量统计有独立可分享的运维入口", () => {
  assert.equal(adminRouteOf("#totals?from=2026-11-01T00%3A00%3A00Z").tab, "totals");
  for (const role of ["admin", "operator", "viewer"]) assert.equal(canSeeAdminTab("totals", role), true);
});

test("租户详情的入口与顶部菜单可分享且保留来源", () => {
  const route = adminRouteOf(tenantHash("tenant/a & b", "billing", "billing"));
  assert.equal(route.tab, "tenants");
  assert.equal(route.query.get("tenant"), "tenant/a & b");
  assert.equal(route.query.get("view"), "billing");
  assert.equal(route.query.get("from"), "billing");
  assert.equal(tenantViewOf(route.query.get("view")), "billing");
  assert.equal(tenantViewOf("unknown"), "basic");
  assert.equal(tenantViewOf(null), "basic");
  assert.equal(adminRouteOf(tenantHash("t")).query.get("view"), "basic");
});

test("旧租户计费与方案链接进入同一个租户页", () => {
  for (const [hash, view, source] of [
    ["#billing?section=accounts&tenant=t1", "billing", "billing"],
    ["#pricing?section=plans&tenant=t1", "discounts", "pricing"],
  ]) {
    const route = adminRouteOf(hash!);
    assert.equal(route.tab, "tenants");
    assert.equal(route.query.get("tenant"), "t1");
    assert.equal(route.query.get("view"), view);
    assert.equal(route.query.get("from"), source);
    assert.equal(route.query.has("section"), false);
  }
  assert.equal(adminRouteOf("#billing?section=topups&tenant=t1").tab, "billing");
});
