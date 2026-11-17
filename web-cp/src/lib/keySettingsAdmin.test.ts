import assert from "node:assert/strict";
import { test } from "node:test";
import { adminKeySettingsPatch } from "./keySettingsAdmin.ts";

const original = {
  label: "existing", model_ids: ["model-a"], allowed_models: ["special-*"] as string[] | null,
  group: { id: "group-a" }, rate_limit: { concurrency: 2, rpm: 12 }, disabled: false,
  expires_at: "2032-01-02T03:04:05Z" as string | null,
};
const draft = {
  label: "existing", scope: "patterns" as "inherited" | "selected" | "patterns", modelIds: ["model-a"], patterns: ["special-*"],
  groupId: "group-a", rateLimit: { concurrency: 2, rpm: 12 }, disabled: false, expiresAt: original.expires_at,
};

test("修改名称不清除既有通配授权或其他配置", () => {
  assert.deepEqual(adminKeySettingsPatch(original, { ...draft, label: "renamed" }), { label: "renamed" });
  assert.deepEqual(adminKeySettingsPatch(original, draft), {});
});

test("指定模型会显式清除优先级更高的旧通配配置", () => {
  assert.deepEqual(adminKeySettingsPatch(original, { ...draft, scope: "selected", modelIds: ["model-b"] }), { model_ids: ["model-b"], allowed_models: null });
});

test("不限制模型清除两种覆盖，取消限制使用null且固定分组不重发", () => {
  assert.deepEqual(adminKeySettingsPatch(original, { ...draft, scope: "inherited", rateLimit: {}, expiresAt: null }), {
    model_ids: null, allowed_models: null, rate_limit: null, expires_at: null,
  });
});

test("旧无分组密钥可以补选一次，模型集合顺序不制造无效更新", () => {
  const source = { ...original, model_ids: ["model-a", "model-b"], allowed_models: null, group: null };
  assert.deepEqual(adminKeySettingsPatch(source, { ...draft, scope: "selected", modelIds: ["model-b", "model-a"] }), { group_id: "group-a" });
});
