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

test("成员与密钥详情保留主导航并解码自己的标识", () => {
  assert.deepEqual(portalRouteOf("#members?member=member%2Fa"), { tab: "members", group: null, memberId: "member/a" });
  assert.deepEqual(portalRouteOf("#keys?key=key%2Fa&edit=1"), { tab: "keys", group: null, keyId: "key/a" });
  assert.deepEqual(portalRouteOf("#keys?key=key-a&from=2026-11-01T00%3A00%3A00Z"), { tab: "keys", group: null, keyId: "key-a" });
});

test("详情标识不污染其它页、空值或非法主导航", () => {
  assert.deepEqual(portalRouteOf("#keys?member=member-a"), { tab: "keys", group: null });
  assert.deepEqual(portalRouteOf("#members?key=key-a"), { tab: "members", group: null });
  assert.deepEqual(portalRouteOf("#keys?key="), { tab: "keys", group: null });
  assert.deepEqual(portalRouteOf("#members?member="), { tab: "members", group: null });
  assert.deepEqual(portalRouteOf("#invalid?key=key-a&member=member-a"), { tab: "org", group: null });
});

test("成员详情不扩大普通成员或账单角色的管理入口", () => {
  for (const role of ["member", "billing", "future_role"]) {
    for (const status of ["active", "suspended", "pending"]) {
      assert.equal(canSeePortalTab("members", role, status), false);
    }
  }
  assert.equal(canSeePortalTab("members", "owner", "active"), true);
  assert.equal(canSeePortalTab("members", "admin", "active"), true);
});
