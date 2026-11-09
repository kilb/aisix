import assert from "node:assert/strict";
import { test } from "node:test";
import { MAX_UPSTREAM_CHARS, upstreamNameProblem } from "./upstreamName.ts";

test("an empty field is not a refusal: it means the public name is sent", () => {
  assert.equal(upstreamNameProblem(""), null);
  assert.equal(upstreamNameProblem("   "), null);
});

test("a name of any shape the control plane takes passes, listed or not", () => {
  for (const name of ["gpt-4o", "claude-sonnet-4-5", "deepseek/deepseek-chat", "a", "x".repeat(MAX_UPSTREAM_CHARS)]) {
    assert.equal(upstreamNameProblem(name), null, name);
  }
  // The failure a membership check would have made: a name no list carries.
  assert.equal(upstreamNameProblem("not-in-any-list-5e1"), null);
});

test("one character past the limit is refused, and the message names the limit", () => {
  const problem = upstreamNameProblem("x".repeat(MAX_UPSTREAM_CHARS + 1));
  assert.equal(problem, `上游模型名称不能超过 ${MAX_UPSTREAM_CHARS} 个字符`);
});

test("a control character is refused, at any position it survives trimming", () => {
  // A trailing tab is whitespace and `trim` removes it, as the control plane's
  // own check does; these are the ones that remain.
  for (const name of ["a\u0000b", "\u001fb", "ab\u007f", "a\u0001b"]) {
    assert.equal(upstreamNameProblem(name), "上游模型名称不能包含控制字符", JSON.stringify(name));
  }
});

test("surrounding whitespace is not counted: it is trimmed before the limit", () => {
  assert.equal(upstreamNameProblem(`  ${"x".repeat(MAX_UPSTREAM_CHARS)}  `), null);
});
