import assert from "node:assert/strict";
import { test } from "node:test";
import {
  MAX_PUBLIC_NAME_CHARS,
  appliedText,
  publicNameProblem,
  selectable,
  stateOf,
  stateText,
  type AccountRow,
} from "./upstreamBulk.ts";

const row = (public_model_name: string, upstream_model: string | null, served = true, enabled = true): AccountRow => ({
  public_model_name,
  upstream_model,
  served,
  enabled,
});

test("a name the control plane takes as a public model's passes", () => {
  for (const name of ["gpt-4o", "deepseek/deepseek-chat", "ft:gpt-4o:org:x", "x".repeat(MAX_PUBLIC_NAME_CHARS), "模型-1"]) {
    assert.equal(publicNameProblem(name), null, name);
  }
});

test("each shape a public model's name cannot have is refused", () => {
  assert.equal(publicNameProblem(""), "名称为空");
  assert.equal(publicNameProblem("  "), "名称为空");
  assert.equal(publicNameProblem("x".repeat(MAX_PUBLIC_NAME_CHARS + 1)), `名称超过 ${MAX_PUBLIC_NAME_CHARS} 个字符`);
  // Counted in characters, not UTF-16 units: 128 astral characters fit.
  assert.equal(publicNameProblem("𝔸".repeat(MAX_PUBLIC_NAME_CHARS)), null);
  for (const name of ["a\u0000b", "a\u007fb", "a\u0085b"]) {
    assert.equal(publicNameProblem(name), "名称包含控制字符", JSON.stringify(name));
  }
  assert.equal(publicNameProblem("gpt-*"), "名称包含 *");
  assert.equal(publicNameProblem("a#b"), "名称包含 #");
});

test("an id the account already maps is mapped, under the public model that maps it", () => {
  const rows = [row("alias-b", "gpt-4o", true, false), row("alias-a", "gpt-4o"), row("gpt-4o", null, false)];
  const state = stateOf("gpt-4o", rows, new Set(["alias-a", "alias-b", "gpt-4o"]), new Set());
  // The first by public name, as the control plane answers it.
  assert.deepEqual(state, { kind: "mapped", publicName: "alias-a", enabled: true });
  assert.equal(selectable(state), false);
  // A row without an upstream name asks for the public name itself.
  assert.deepEqual(stateOf("o1", [row("o1", null, true, false)], new Set(["o1"]), new Set()), {
    kind: "mapped",
    publicName: "o1",
    enabled: false,
  });
  assert.equal(stateText({ kind: "mapped", publicName: "o1", enabled: false }), "已映射：公共模型 o1（未启用）");
});

test("a public model the account serves under another upstream name is left alone", () => {
  const state = stateOf("gpt-4o", [row("gpt-4o", "gpt-4o-2024")], new Set(["gpt-4o"]), new Set());
  assert.deepEqual(state, { kind: "served", upstream: "gpt-4o-2024", enabled: true });
  assert.equal(selectable(state), false);
});

test("an unmapped id reuses the live public model of its name, or creates one", () => {
  const rows = [row("gpt-4o", null, false), row("other", "other-up")];
  const reuse = stateOf("gpt-4o", rows, new Set(["gpt-4o", "other"]), new Set());
  assert.deepEqual(reuse, { kind: "reuse" });
  assert.equal(selectable(reuse), true);
  const create = stateOf("gpt-4.1", rows, new Set(["gpt-4o", "other"]), new Set());
  assert.deepEqual(create, { kind: "create" });
  assert.equal(selectable(create), true);
});

test("a retired name and a name a public model cannot have are not offered", () => {
  const retired = stateOf("old-1", [], new Set(), new Set(["old-1"]));
  assert.deepEqual(retired, { kind: "retired" });
  assert.equal(selectable(retired), false);
  const invalid = stateOf("a#b", [], new Set(), new Set());
  assert.deepEqual(invalid, { kind: "invalid", problem: "名称包含 #" });
  assert.equal(selectable(invalid), false);
});

test("the applied sentence counts each outcome", () => {
  const item = (outcome: "created" | "added" | "unchanged") => ({
    upstream_model: "m",
    public_model_id: "id",
    public_model_name: "m",
    outcome,
  });
  assert.equal(
    appliedText([item("created"), item("created"), item("added")]),
    "已映射 3 个上游模型：新建公共模型 2 个，复用已有公共模型 1 个。",
  );
  assert.equal(
    appliedText([item("added"), item("unchanged")]),
    "已映射 1 个上游模型：新建公共模型 0 个，复用已有公共模型 1 个，1 个已映射，保持不变。",
  );
});
