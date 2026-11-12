import assert from "node:assert/strict";
import { test } from "node:test";
import { canSeePortalTab, portalRouteOf } from "./navigation.ts";

// 账号入口不能继承组织账务权限；回归会让待审核成员失去改密入口。
for (const role of ["owner", "admin", "member", "billing", "future_role"]) {
  for (const status of ["pending", "active", "suspended"]) {
    test(`${role}/${status} 可打开自己的账号设置`, () => {
      assert.equal(canSeePortalTab("account", role, status), true);
    });
  }
}

test("普通成员的个人账号不授予组织账务访问", () => {
  assert.equal(canSeePortalTab("billing", "member", "active"), false);
  assert.equal(canSeePortalTab("billing", "owner", "pending"), false);
  assert.equal(canSeePortalTab("billing", "billing", "active"), true);
});

test("账号与已有价格分组深链保持独立", () => {
  assert.deepEqual(portalRouteOf("#account"), { tab: "account", group: null });
  assert.deepEqual(portalRouteOf("#prices?group=group%2Fa"), { tab: "prices", group: "group/a" });
  assert.deepEqual(portalRouteOf("#billing"), { tab: "billing", group: null });
  assert.deepEqual(portalRouteOf("#unknown"), { tab: "org", group: null });
});

test("用量分析和独立调用记录均可直达", () => {
  assert.equal(portalRouteOf("#usage?view=cache&api_key_id=key-a").tab, "usage");
  assert.equal(portalRouteOf("#calls?model_id=model-a").tab, "calls");
});

test("调用记录沿用用量权限，待审和停用组织不可扩大访问", () => {
  for (const role of ["owner", "admin", "member", "billing"]) {
    assert.equal(canSeePortalTab("calls", role, "active"), true);
    assert.equal(canSeePortalTab("calls", role, "pending"), false);
    assert.equal(canSeePortalTab("calls", role, "suspended"), false);
  }
  assert.equal(canSeePortalTab("calls", "future_role", "active"), false);
});
