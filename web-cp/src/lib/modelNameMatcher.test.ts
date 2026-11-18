import assert from "node:assert/strict";
import { test } from "node:test";
import { modelNameMatcher } from "./modelNameMatcher.ts";

test("普通搜索保留子串，通配符按完整模型名匹配，其他字符保持字面值", () => {
  const models = ["GPT-4o", "gpt-4o-mini", "embed-gpt-4o", "gpt-5", "a.b[x]", "aba", "ababa"];
  const match = (pattern: string) => models.filter(modelNameMatcher(pattern));
  assert.deepEqual(match("4O"), models.slice(0, 3));
  assert.deepEqual(match("  gpt-4o*  "), models.slice(0, 2));
  assert.deepEqual(match("*GPT*4O"), [models[0], models[2]]);
  assert.deepEqual(match("a.b[*]"), ["a.b[x]"]);
  assert.deepEqual(match("aba*aba"), []);
  assert.deepEqual(match("a**b*a"), ["aba", "ababa"]);
  assert.deepEqual(match("*"), models);
});
