import assert from "node:assert/strict";
import { test } from "node:test";
import { portalEntryOf, portalPagePath } from "./portalEntry.ts";

test("根路径和公开价格锚点进入公开首页", () => {
  assert.equal(portalEntryOf("/", "", "/"), "home");
  assert.equal(portalEntryOf("/", "#pricing", "/"), "home");
  assert.equal(portalEntryOf("/tenant/", "#pricing", "/tenant/"), "home");
});

test("保留旧控制台深链和带前缀的独立入口", () => {
  assert.equal(portalEntryOf("/", "#prices?group=one", "/"), "console");
  assert.equal(portalEntryOf("/tenant/console", "#calls", "/tenant/"), "console");
  assert.equal(portalEntryOf("/tenant/login/", "", "/tenant/"), "login");
  assert.equal(portalEntryOf("/tenant/signup", "", "/tenant/"), "signup");
  assert.equal(portalEntryOf("/tenant/invite", "#token=", "/tenant/"), "invite");
  assert.equal(portalPagePath("/tenant/", "console", "#keys?key=one"), "/tenant/console#keys?key=one");
});
