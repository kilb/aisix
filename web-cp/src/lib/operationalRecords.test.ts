import assert from "node:assert/strict";
import { test } from "node:test";
import { keySearchText, resourceSearchText, auditSearchText, cursorPageAfterLoad } from "./operationalRecords.ts";

test("密钥检索只读取公开标识和分组，不包含secret或hash", () => {
  const row = { id: "id-1", label: "研发", prefix: "sk-visible", group: { name: "team", display_name: "开发组" }, secret: "DO-NOT-INDEX", key_hash: "HASH-SECRET" };
  const text = keySearchText(row).join(" ");
  assert.match(text, /研发/);
  assert.match(text, /sk-visible/);
  assert.match(text, /开发组/);
  assert.doesNotMatch(text, /DO-NOT-INDEX|HASH-SECRET/);
});

test("资源检索包含name/display_name/id，不搜索原始文档与凭据", () => {
  const row = { id: "resource-1", doc: { name: "规则", display_name: "显示名称", api_key: "SECRET-RESOURCE", extra_headers: { Authorization: "TOKEN" } } };
  const text = resourceSearchText(row).join(" ");
  assert.match(text, /resource-1/);
  assert.match(text, /规则/);
  assert.match(text, /显示名称/);
  assert.doesNotMatch(text, /SECRET-RESOURCE|TOKEN|Authorization/);
});

test("审计检索使用公开actor/action/target，不检索detail", () => {
  const row = { id: 91, actor: { kind: "operator", id: "op-1", email: "ops@example.com" }, action: "updated", target_kind: "tenant", target_id: "tenant-1", tenant_id: "tenant-1", detail: { password: "SECRET-AUDIT" } };
  const text = auditSearchText(row).join(" ");
  assert.match(text, /ops@example.com/);
  assert.match(text, /tenant-1/);
  assert.doesNotMatch(text, /SECRET-AUDIT|password/);
});


test("游标加载先填满当前搜索页，不跨过刚加载的匹配结果", () => {
  assert.equal(cursorPageAfterLoad(1, 25, 0, 2), 1);
  assert.equal(cursorPageAfterLoad(1, 25, 1, 2), 1);
  assert.equal(cursorPageAfterLoad(2, 25, 40, 3), 2);
  assert.equal(cursorPageAfterLoad(2, 25, 50, 3), 3);
});
