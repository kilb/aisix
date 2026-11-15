import assert from "node:assert/strict";
import test from "node:test";
import { keySettingsPatch, type KeySettings } from "./keySettings.ts";

const current = {
  label: "production", model_ids: ["model-a"], group: { id: "group-a", display_name: "标准" },
  rate_limit: { concurrency: 3, rpm: 60 }, disabled: false,
  expires_at: "2026-12-01T00:00:00Z",
};
const settings: KeySettings = {
  label: "production", modelIds: ["model-a"], groupId: "group-a",
  rateLimit: { concurrency: 3, rpm: 60 }, disabled: false,
  expiresAt: "2026-12-01T00:00:00.000Z",
};

test("unchanged settings never resend immutable group or unchanged restrictions", () => {
  assert.deepEqual(keySettingsPatch(current, settings), {});
});

test("editing a name cannot truncate the existing expiry's fractional second", () => {
  assert.deepEqual(keySettingsPatch({ ...current, expires_at: "2026-12-01T00:00:00.123Z" }, { ...settings, label: "renamed" }), { label: "renamed" });
});

test("clearing overrides explicitly restores inherited models, limits and permanent validity", () => {
  assert.deepEqual(keySettingsPatch(current, { ...settings, modelIds: null, rateLimit: {}, expiresAt: null }), {
    model_ids: null, rate_limit: null, expires_at: null,
  });
});

test("saving a different setting cannot reset another existing restriction", () => {
  assert.deepEqual(keySettingsPatch(current, { ...settings, label: "renamed", rateLimit: { concurrency: 3 }, disabled: true }), {
    label: "renamed", rate_limit: { concurrency: 3 }, disabled: true,
  });
});

test("only a legacy key without a group can receive its one-time selection", () => {
  assert.deepEqual(keySettingsPatch(current, { ...settings, groupId: "group-b" }), {});
  assert.deepEqual(keySettingsPatch({ ...current, group: null }, settings), { group_id: "group-a" });
});

test("model order alone is no permission change; an explicitly empty legacy scope is preserved", () => {
  assert.deepEqual(keySettingsPatch({ ...current, model_ids: ["model-b", "model-a"] }, { ...settings, modelIds: ["model-a", "model-b"] }), {});
  assert.deepEqual(keySettingsPatch({ ...current, model_ids: [] }, { ...settings, modelIds: [] }), {});
});
